//! Real process, Unix socket and PTY checks; run with cargo test.
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt, symlink},
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

// Reuse the production Unix boundary instead of duplicating unsafe PTY wrappers.
#[allow(dead_code)]
#[path = "../src/os.rs"]
mod os;

#[allow(dead_code)]
#[path = "../src/history.rs"]
mod history;

#[allow(dead_code)]
#[path = "../src/logfile.rs"]
mod logfile;
mod storage {
    pub fn open_file(path: &std::path::Path, _: bool) -> std::io::Result<std::fs::File> {
        std::fs::File::open(path)
    }
}
fn decoded(path: impl AsRef<std::path::Path>) -> Vec<u8> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    logfile::read(&mut file, 256 * 1024 * 1024, true)
        .unwrap()
        .bytes
}

#[path = "support/screen.rs"]
mod screen;

const BINARY: &str = env!("CARGO_BIN_EXE_rtch");
static NEXT: AtomicUsize = AtomicUsize::new(0);
const CAPABILITIES: u8 = 8;
const ATTACH_FREE: u8 = 9;

fn packet(kind: u8, bytes: &[u8]) -> [u8; 10] {
    let mut packet = [0; 10];
    packet[0] = kind;
    packet[1] = u8::try_from(bytes.len()).unwrap();
    packet[2..2 + bytes.len()].copy_from_slice(bytes);
    packet
}

fn protocol_connection(listener: &UnixListener) -> (UnixStream, [u8; 10]) {
    listener.set_nonblocking(true).unwrap();
    let mut connection = None;
    wait_for(|| match listener.accept() {
        Ok((mut socket, _)) => {
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut request = [0; 10];
            if socket.read_exact(&mut request).is_ok() {
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                connection = Some((socket, request));
            }
            connection.is_some()
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("accept failed: {error}"),
    });
    connection.unwrap()
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "condition timed out");
        sleep(Duration::from_millis(20));
    }
}
fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle)
}
fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap().trim()
}
fn signal_child(child: &Child, signal: &str) {
    assert!(
        Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
fn stopped(child: &Child) -> bool {
    fs::read(format!("/proc/{}/status", child.id()))
        .is_ok_and(|status| contains(&status, b"State:\tT"))
}
struct Process(Child);
impl Process {
    fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        wait_for(|| {
            status = self.0.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
#[allow(clippy::struct_field_names)]
struct Sessions {
    root: PathBuf,
    sessions: PathBuf,
    tracked_jobs: Vec<os::ChildSession>,
}
impl Sessions {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rtch-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let sessions = root.join("sessions");
        fs::create_dir(&sessions).unwrap();
        fs::create_dir_all(root.join("config/rtch")).unwrap();
        let suite = Self {
            root,
            sessions,
            tracked_jobs: Vec::new(),
        };
        suite.config("");
        suite
    }
    fn config(&self, extra: &str) {
        fs::write(
            self.root.join("config/rtch/config"),
            format!("session_dir = {}\n{extra}", self.sessions.display()),
        )
        .unwrap();
    }
    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", &self.root)
            .env("SHELL", "/bin/bash")
            .env_remove("RTCH_SESSION")
            .env_remove("ATCH_SESSION")
            .env_remove("COMPLETE");
        cmd
    }
    fn output(&self, cmd: &mut Command, input: &[u8]) -> Output {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let stdin = self.root.join(format!("{id}.in"));
        let stdout = self.root.join(format!("{id}.out"));
        let stderr = self.root.join(format!("{id}.err"));
        fs::write(&stdin, input).unwrap();
        let mut child = Process(
            cmd.stdin(File::open(stdin).unwrap())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        Output {
            status: child.wait(),
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }
    fn run(&self, args: &[&str], input: &[u8]) -> Output {
        self.output(self.command(BINARY).args(args), input)
    }
    fn ok(&self, args: &[&str]) -> Vec<u8> {
        let out = self.run(args, b"");
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        out.stdout
    }
    fn push(&self, name: &str, bytes: &[u8]) {
        assert!(self.run(&["push", name], bytes).status.success());
    }
    fn ended(&self, name: &str) {
        wait_for(|| {
            self.sessions.join(format!("{name}.ended")).exists()
                && !self.sessions.join(name).exists()
        });
    }
    fn log(&self, name: &str) -> Vec<u8> {
        decoded(self.sessions.join(format!("{name}.log")))
    }
    fn track_jobs(&mut self, name: &str) {
        let socket = UnixStream::connect(self.sessions.join(name)).unwrap();
        self.tracked_jobs
            .push(os::ChildSession::identify(&socket).unwrap());
    }
    fn attach(&self, args: &[&str]) -> Client {
        Self::terminal(
            self.command(BINARY)
                .env("TERM", "xterm-256color")
                .arg("-q")
                .args(args),
        )
    }
    fn terminal(command: &mut Command) -> Client {
        let (master, slave) = os::pty(
            None,
            &libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        let original = os::term(slave.as_raw_fd()).unwrap();
        let stdout_flags = os::flags(slave.as_raw_fd()).unwrap();
        let child = command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap())
            .spawn()
            .unwrap();
        Client {
            child: Process(child),
            master,
            slave,
            original,
            stdout_flags,
            flag_probe: None,
        }
    }
    fn complete(&self, words: &[&str]) -> Vec<String> {
        let out = self.output(
            self.command(BINARY)
                .arg("--")
                .args(words)
                .env("COMPLETE", "bash")
                .env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
                .env("_CLAP_COMPLETE_COMP_TYPE", "9")
                .env("_CLAP_COMPLETE_SPACE", "false")
                .env("_CLAP_IFS", "\n"),
            b"",
        );
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout).lines().map(str::to_owned).collect()
    }
}
impl Drop for Sessions {
    fn drop(&mut self) {
        // Retain identities so a failed assertion cannot leak background jobs
        // after the supervisor has removed this private session's socket.
        for session in self.tracked_jobs.drain(..) {
            let _ = session.kill();
        }
        if let Ok(entries) = fs::read_dir(&self.sessions) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_socket()) {
                    // Each socket belongs to this test's private directory.
                    if let Ok(child) = self
                        .command(BINARY)
                        .args(["kill", "-f"])
                        .arg(entry.path())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        let mut child = Process(child);
                        let deadline = Instant::now() + Duration::from_secs(3);
                        while matches!(child.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                            sleep(Duration::from_millis(20));
                        }
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
struct Client {
    child: Process,
    master: File,
    slave: File,
    original: libc::termios,
    stdout_flags: i32,
    flag_probe: Option<File>,
}
impl Client {
    fn read_until(&mut self, marker: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        wait_for(|| {
            let mut fds = [libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            os::poll(&mut fds, 50).unwrap();
            if fds[0].revents & libc::POLLIN != 0 {
                let mut buf = [0; 8192];
                let n = self.master.read(&mut buf).unwrap();
                out.extend_from_slice(&buf[..n]);
            }
            contains(&out, marker)
        });
        out
    }
    fn detach(&mut self) {
        self.master.write_all(&[28]).unwrap();
        assert!(self.child.wait().success());
    }
    fn restored(&self) {
        assert_eq!(
            os::flags(self.flag_probe.as_ref().unwrap_or(&self.slave).as_raw_fd()).unwrap(),
            self.stdout_flags
        );
        let t = os::term(self.slave.as_raw_fd()).unwrap();
        let o = &self.original;
        assert_eq!(
            (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_cc),
            (o.c_iflag, o.c_oflag, o.c_cflag, o.c_lflag, o.c_cc)
        );
    }
}

fn picker_reporting_enabled(output: &[u8]) {
    assert!(contains(output, b"\x1b[?1049h\x1b[>5u\x1b[?4m\x1b[>4;2m"));
}

fn picker_reporting_restored(output: &[u8]) {
    picker_reporting_restored_to(output, b"\x1b[>4m");
}

fn picker_reporting_restored_to(output: &[u8], mode: &[u8]) {
    let mut restore = b"\x1b[<u".to_vec();
    restore.extend_from_slice(mode);
    assert!(contains(output, &restore));
    let pop = output
        .windows(4)
        .position(|part| part == b"\x1b[<u")
        .unwrap();
    let screen = output
        .windows(8)
        .position(|part| part == b"\x1b[?1049l")
        .unwrap();
    assert!(
        pop < screen,
        "keyboard modes must be restored before leaving the picker"
    );
}

#[test]
fn ended_and_no_implicit_restart() {
    let s = Sessions::new();
    s.ok(&["start", "finished", "sh", "-c", "printf FINISHED"]);
    s.ended("finished");
    assert!(contains(&s.ok(&["ended"]), b"finished"));
    assert!(contains(&s.ok(&["list"]), b"[ended]"));
    assert!(contains(&s.ok(&["tail", "finished"]), b"FINISHED"));
    for args in [&["attach", "finished"][..], &["finished"]] {
        let out = s.run(args, b"");
        assert!(!out.status.success());
        assert!(!contains(&out.stdout, b"FINISHED"));
        assert!(!s.sessions.join("finished").exists());
    }
    s.ok(&["rm", "-a"]);
    assert!(!s.sessions.join("finished.log").exists());
    assert!(!s.sessions.join("finished.ended").exists());
    assert!(!s.sessions.join("finished.head").exists());
}

#[test]
fn recursive_session_opening_is_rejected_before_terminal_or_session_changes() {
    let s = Sessions::new();
    s.ok(&["start", "occupied", "printf", "PRESERVED_HISTORY"]);
    s.ended("occupied");
    let old_log = s.log("occupied");
    let old_head = decoded(s.sessions.join("occupied.head"));
    let args = [
        vec![],
        vec!["pick"],
        vec!["new", "occupied", "true"],
        vec!["n", "nested", "true"],
        vec!["start", "nested", "true"],
        vec!["s", "nested", "true"],
        vec!["run", "nested", "true"],
        vec!["attach", "occupied"],
        vec!["a", "occupied"],
        vec!["open", "nested", "true"],
        vec!["nested", "true"],
        vec!["__serve", "nested", "true"],
    ];
    for args in args {
        let out = s.output(
            s.command(BINARY).args(&args).env("RTCH_SESSION", "outer"),
            b"",
        );
        assert!(!out.status.success(), "{args:?}");
        assert!(contains(&out.stderr, b"already inside rtch"), "{args:?}");
        assert!(!out.stdout.contains(&27));

        let mut c = Sessions::terminal(
            s.command(BINARY)
                .args(&args)
                .env("TERM", "xterm")
                .env("RTCH_SESSION", "outer"),
        );
        let out = c.read_until(b"already inside rtch");
        assert!(!out.contains(&27));
        assert!(!c.child.wait().success(), "{args:?}");
        c.restored();
        assert!(!s.sessions.join("nested").exists());
        assert!(!s.sessions.join("nested.log").exists());
        assert!(!s.sessions.join("nested.head").exists());
        assert!(!s.sessions.join("occupied").exists());
        assert_eq!(s.log("occupied"), old_log);
        assert_eq!(decoded(s.sessions.join("occupied.head")), old_head);
        assert!(s.sessions.join("occupied.ended").exists());
    }

    let out = s.output(
        s.command(BINARY).env("RTCH_SESSION", "").args([
            "start",
            "empty-marker",
            "printf",
            "EMPTY_MARKER_ALLOWED",
        ]),
        b"",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    s.ended("empty-marker");
    assert!(contains(&s.log("empty-marker"), b"EMPTY_MARKER_ALLOWED"));
    let mut c = Sessions::terminal(
        s.command(BINARY)
            .args(["-q", "pick"])
            .env("TERM", "xterm")
            .env("RTCH_SESSION", ""),
    );
    c.read_until(b"scroll");
    c.detach();
    c.restored();
}

#[test]
fn session_management_remains_available_inside_rtch() {
    let s = Sessions::new();
    s.ok(&[
        "start",
        "managed",
        "sh",
        "-c",
        "printf 'MANAGED_READY\\n'; while IFS= read -r line; do printf 'MANAGED_%s\\n' \"$line\"; done",
    ]);
    wait_for(|| contains(&s.log("managed"), b"MANAGED_READY"));
    let marker = s.sessions.join("managed");
    let manage = |args: &[&str], input: &[u8]| {
        let out = s.output(
            s.command(BINARY).args(args).env("RTCH_SESSION", &marker),
            input,
        );
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        out.stdout
    };
    assert!(contains(&manage(&["list"], b""), b"managed"));
    assert_eq!(text(&manage(&["current"], b"")), "managed");
    assert!(contains(
        &manage(&["tail", "managed"], b""),
        b"MANAGED_READY"
    ));
    manage(&["push", "managed"], b"nested-management\n");
    wait_for(|| contains(&s.log("managed"), b"MANAGED_nested-management"));
    let mut c = s.attach(&["attach", "managed"]);
    c.read_until(b"MANAGED_READY");
    manage(&["clear"], b"");
    wait_for(|| s.log("managed").is_empty());
    manage(&["detach", "managed"], b"");
    assert!(c.child.wait().success());
    c.restored();
    manage(&["kill", "-f", "managed"], b"");
    s.ended("managed");
    assert!(contains(&manage(&["ended"], b""), b"managed"));
    assert!(contains(&manage(&["shell-init"], b""), b"RTCH_SESSION"));
    manage(&["rm", "managed"], b"");
    assert!(!s.sessions.join("managed.log").exists());
}

#[test]
fn default_shell_loads_profile_functions_once_and_reattach_keeps_them() {
    let s = Sessions::new();
    fs::write(s.root.join(".profile"),
        "export RTCH_PROFILE_TEST=loaded\nprintf x >> \"$HOME/profile-count\"\nz() { printf 'PROFILE_%s_FUNCTION\\n' \"$RTCH_PROFILE_TEST\"; }\n").unwrap();
    let mut c = s.attach(&["profile"]);
    wait_for(|| s.root.join("profile-count").exists() && s.sessions.join("profile").exists());
    s.push("profile", b"z\n");
    c.read_until(b"PROFILE_loaded_FUNCTION");
    c.detach();
    let mut c = s.attach(&["attach", "profile"]);
    s.push(
        "profile",
        b"z; printf 'AGAIN_%s\\n' \"$RTCH_PROFILE_TEST\"\n",
    );
    c.read_until(b"AGAIN_loaded");
    assert_eq!(fs::read(s.root.join("profile-count")).unwrap(), b"x");
    c.detach();

    // Explicit programs keep their own startup semantics and literal arguments.
    s.ok(&[
        "start",
        "explicit",
        "printf",
        "%s",
        "literal $HOME; 'argument'",
    ]);
    s.ended("explicit");
    assert_eq!(
        text(&s.ok(&["tail", "explicit"])),
        "literal $HOME; 'argument'"
    );
    assert_eq!(fs::read(s.root.join("profile-count")).unwrap(), b"x");
}

#[test]
fn batched_push_preserves_packets_after_writer_disconnects() {
    let s = Sessions::new();
    s.ok(&[
        "start",
        "batch",
        "sh",
        "-c",
        "stty raw -echo; printf READY; head -c 32771; sleep 60",
    ]);
    wait_for(|| contains(&s.log("batch"), b"READY"));
    let payload: Vec<_> = (0..32771)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect();
    s.push("batch", &payload);
    wait_for(|| s.log("batch").len() >= 5 + payload.len());
    assert_eq!(&s.log("batch")[5..], payload);
}
#[test]
fn detach_hup_resets_keyboard_and_termios() {
    let s = Sessions::new();
    let mut c = s.attach(&["work", "sh", "-c", r"printf '\033[>7uREADY'; sleep 60"]);
    c.read_until(b"READY");
    assert!(
        Command::new("kill")
            .args(["-HUP", &c.child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let out = c.read_until(b"\x1b[?25h");
    c.child.wait();
    assert!(contains(&out, b"\x1b[=0u"));
    assert!(contains(&out, b"\x1b[>4;0m"));
    c.restored();
    assert!(s.sessions.join("work").exists());
    let mut c = s.attach(&["attach", "work"]);
    c.read_until(b"READY");
    s.ok(&["detach", "work"]);
    c.read_until(b"\x1b[=0u");
    assert!(c.child.wait().success());
    c.restored();
}
#[test]
fn replay_filters_queries_but_live_is_transparent() {
    let s = Sessions::new();
    let mut c = s.attach(&[
        "queries",
        "sh",
        "-c",
        r"printf '\033[6n\033]10;?\033\\\033[?u\033[cREADY'; sleep 60",
    ]);
    assert!(contains(&c.read_until(b"READY"), b"\x1b[6n"));
    c.detach();
    let mut c = s.attach(&["attach", "queries"]);
    let out = c.read_until(b"READY");
    for query in [b"\x1b[6n".as_slice(), b"\x1b]10;", b"\x1b[?u", b"\x1b[c"] {
        assert!(!contains(&out, query));
    }
    assert!(!contains(&s.ok(&["tail", "queries"]), b"\x1b[6n"));
}
#[test]
fn clear_has_no_nul_hole() {
    let s = Sessions::new();
    s.ok(&["start", "echo", "sh", "-c", "stty -echo; printf READY; cat"]);
    wait_for(|| contains(&s.log("echo"), b"READY"));
    s.push("echo", b"BEFORE\n");
    wait_for(|| contains(&s.log("echo"), b"BEFORE"));
    s.ok(&["clear", "echo"]);
    s.push("echo", b"AFTER\n");
    wait_for(|| contains(&s.log("echo"), b"AFTER"));
    assert!(!s.log("echo").contains(&0));
    assert!(!contains(&s.log("echo"), b"BEFORE"));
}
#[test]
fn kill_foreground_and_shell() {
    let s = Sessions::new();
    let mut c = s.attach(&["shell", "bash", "--noprofile", "--norc", "-i"]);
    wait_for(|| s.sessions.join("shell").exists());
    s.push("shell", b"printf READY; sleep 60\n");
    c.read_until(b"READY");
    sleep(Duration::from_millis(200));
    s.ok(&["kill", "-f", "shell"]);
    s.ended("shell");
}
#[test]
fn fragmented_protocol_and_slow_client() {
    let s = Sessions::new();
    s.ok(&[
        "start",
        "slow",
        "sh",
        "-c",
        "sleep 0.3; head -c 2000000 /dev/zero; sleep 60",
    ]);
    let mut stream = UnixStream::connect(s.sessions.join("slow")).unwrap();
    stream.write_all(&[1, 0, 0]).unwrap();
    sleep(Duration::from_millis(50));
    stream.write_all(&[0; 7]).unwrap();
    sleep(Duration::from_millis(600));
    s.ok(&["kill", "-f", "slow"]);
    s.ended("slow");
}
#[test]
fn log_symlink_rejected() {
    let s = Sessions::new();
    let victim = s.root.join("victim");
    fs::write(&victim, b"PRIVATE").unwrap();
    symlink(&victim, s.sessions.join("bad.log")).unwrap();
    assert!(
        !s.run(&["start", "bad", "sleep", "60"], b"")
            .status
            .success()
    );
    assert_eq!(fs::read(victim).unwrap(), b"PRIVATE");
}
#[test]
fn log_disabled_still_has_ended_state() {
    let s = Sessions::new();
    s.ok(&["start", "-C", "0", "no-log", "true"]);
    s.ended("no-log");
    assert!(!s.sessions.join("no-log.log").exists());
    assert!(!s.sessions.join("no-log.head").exists());
    assert!(contains(&s.ok(&["ended"]), b"no-log"));
}
#[test]
fn long_directory_and_original_working_directory() {
    let mut s = Sessions::new();
    s.sessions = s.sessions.join("a".repeat(70)).join("b".repeat(70));
    fs::create_dir_all(&s.sessions).unwrap();
    s.config("");
    s.ok(&["start", "pwd", "pwd"]);
    s.ended("pwd");
    assert_eq!(
        text(&s.ok(&["tail", "pwd"])),
        std::env::current_dir().unwrap().to_str().unwrap()
    );
}
#[test]
fn restart_requires_explicit_new() {
    let s = Sessions::new();
    s.ok(&["start", "again", "true"]);
    s.ended("again");
    let mut c = s.attach(&["new", "again", "sh", "-c", "printf RESTARTED; sleep 60"]);
    c.read_until(b"RESTARTED");
    assert!(!s.sessions.join("again.ended").exists());
    s.ok(&["detach", "again"]);
    assert!(c.child.wait().success());
}
#[test]
fn no_log_replay_and_nested_environment() {
    let s = Sessions::new();
    let mut c = s.attach(&[
        "-C",
        "0",
        "ring",
        "sh",
        "-c",
        r#"printf '%s\n' "$RTCH_SESSION"; printf '\033[6nRING_READY'; sleep 60"#,
    ]);
    let out = c.read_until(b"RING_READY");
    assert!(
        contains(
            &out,
            s.sessions
                .join("ring")
                .canonicalize()
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
        ),
        "{}",
        text(&out)
    );
    c.detach();
    let mut c = s.attach(&["attach", "ring"]);
    assert!(!contains(&c.read_until(b"RING_READY"), b"\x1b[6n"));
    assert!(!s.sessions.join("ring.log").exists());
}
#[test]
fn config_defaults_and_overrides() {
    let s = Sessions::new();
    s.config("quiet = true\nlog_size = 0\ntail_lines = 1\n");
    assert!(s.ok(&["start", "silent", "true"]).is_empty());
    s.ended("silent");
    assert!(!s.sessions.join("silent.log").exists());
    s.ok(&["start", "-C", "1k", "logged", "printf", "first\nlast\n"]);
    s.ended("logged");
    assert_eq!(text(&s.ok(&["tail", "logged"])), "last");
    assert!(contains(&s.ok(&["tail", "-n", "2", "logged"]), b"first"));
    assert!(contains(&s.ok(&["list"]), b"logged"));
}
#[test]
fn bad_config_and_explicit_path_override() {
    let s = Sessions::new();
    s.config("session_dir = relative/path\n");
    assert!(!s.run(&["list"], b"").status.success());
    s.ok(&[
        "start",
        s.sessions.join("explicit").to_str().unwrap(),
        "true",
    ]);
    s.ended("explicit");
}
#[test]
fn clear_acknowledged_and_rotation_bounded() {
    let s = Sessions::new();
    s.ok(&[
        "start",
        "-C",
        "1k",
        "rotate",
        "sh",
        "-c",
        "head -c 30000 /dev/zero; printf READY; sleep 60",
    ]);
    wait_for(|| contains(&s.log("rotate"), b"READY"));
    assert!(
        fs::metadata(s.sessions.join("rotate.log")).unwrap().len()
            + fs::metadata(s.sessions.join("rotate.head")).unwrap().len()
            <= 1024
    );
    s.ok(&["clear", "rotate"]);
    assert!(s.log("rotate").is_empty());
    assert!(decoded(s.sessions.join("rotate.head")).is_empty());
}
#[test]
fn run_returns_child_exit_status() {
    let s = Sessions::new();
    assert_eq!(
        s.run(&["run", "status", "sh", "-c", "exit 37"], b"")
            .status
            .code(),
        Some(37)
    );
    assert!(
        fs::read_to_string(s.sessions.join("status.ended"))
            .unwrap()
            .contains("exit=37")
    );
}
#[test]
fn child_keeps_callers_umask() {
    let s = Sessions::new();
    let expected = s.output(s.command("sh").args(["-c", "umask"]), b"");
    s.ok(&["start", "mask", "sh", "-c", "umask"]);
    s.ended("mask");
    assert_eq!(text(&s.ok(&["tail", "mask"])), text(&expected.stdout));
}
#[test]
fn bash_completion_commands_options_and_sessions() {
    let s = Sessions::new();
    s.ok(&["start", "live space", "sleep", "60"]);
    s.ok(&["start", "finished", "true"]);
    s.ended("finished");
    for (words, expected) in [
        (vec!["rtch", "at"], "attach"),
        (vec!["rtch", "-r", "w"], "winch"),
        (vec!["rtch", "kill", "--f"], "--force"),
        (vec!["rtch", "tail", "fin"], "finished"),
    ] {
        assert!(s.complete(&words).contains(&expected.into()));
    }
    let live = s.complete(&["rtch", "attach", ""]);
    assert!(live.contains(&"live space".into()));
    assert!(!live.contains(&"finished".into()));
    let ended = s.complete(&["rtch", "rm", ""]);
    assert!(ended.contains(&"finished".into()));
    assert!(!ended.contains(&"live space".into()));
    let script = s.output(s.command(BINARY).env("COMPLETE", "bash"), b"");
    assert!(script.status.success());
    let path = s.root.join("rtch.bash");
    fs::write(&path, script.stdout).unwrap();
    let out = s.output(s.command("bash").args(["--noprofile", "--norc", "-c",
        "source \"$1\"; COMP_WORDS=(rtch attach li); COMP_CWORD=2; COMP_TYPE=9; _clap_complete_rtch rtch li attach; printf '%s\\n' \"${COMPREPLY[@]}\"", "bash"])
        .arg(path), b"");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "live space");
}

#[test]
fn picker_non_tty_guidance_and_cli_commands() {
    let s = Sessions::new();
    for args in [&[][..], &["pick"][..]] {
        let out = s.run(args, b"");
        assert!(out.status.success());
        assert!(contains(&out.stdout, b"rtch list"));
        assert!(!out.stdout.contains(&27));
    }
    assert!(s.complete(&["rtch", "pi"]).contains(&"pick".into()));
    assert!(
        s.complete(&["rtch", "shell-i"])
            .contains(&"shell-init".into())
    );
}

#[test]
fn picker_recency_selection_english_russian_navigation_and_fragmented_keys() {
    let s = Sessions::new();
    for n in 0..10 {
        s.ok(&[
            "start",
            &format!("slot-{n:02}"),
            "printf",
            &format!("BEGIN-{n}\nEND-{n}\n"),
        ]);
        s.ended(&format!("slot-{n:02}"));
    }
    let mut c = s.attach(&[]);
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    assert!(screen.row(3).contains("> slot-09") && screen.row(3).contains("[ended]"));
    screen.assert_spare_column();
    assert_eq!(
        screen
            .rows()
            .iter()
            .filter(|line| line.contains("slot-"))
            .count(),
        8
    );
    assert!(contains(&out, b"BEGIN-9") && contains(&out, b"END-9"));
    assert!(!contains(&out, b"slot-01") && !contains(&out, b"slot-00"));
    for byte in b"\x1b[B" {
        c.master.write_all(&[*byte]).unwrap();
        sleep(Duration::from_millis(10));
    }
    let out = c.read_until(b"scroll");
    screen.feed(&out);
    assert!(screen.row(4).contains("> slot-08") && screen.row(4).contains("[ended]"));
    assert!(contains(&out, b"BEGIN-8") && contains(&out, b"END-8"));
    assert!(!contains(&out, b"BEGIN-9"));
    c.master.write_all(b"\x1b[A").unwrap();
    let out = c.read_until(b"scroll");
    screen.feed(&out);
    assert!(screen.row(3).contains("> slot-09"));
    assert!(contains(&out, b"BEGIN-9") && contains(&out, b"END-9"));
    c.master.write_all(b"j").unwrap();
    let out = c.read_until(b"scroll");
    screen.feed(&out);
    assert!(screen.row(4).contains("> slot-08"));
    assert!(contains(&out, b"BEGIN-8") && contains(&out, b"END-8"));
    for (key, selected) in [("о", "slot-07"), ("\x1b[A", "slot-08")] {
        for byte in key.to_string().as_bytes() {
            c.master.write_all(&[*byte]).unwrap();
            sleep(Duration::from_millis(10));
        }
        let out = c.read_until(b"scroll");
        screen.feed(&out);
        assert!(
            screen
                .rows()
                .iter()
                .any(|row| row.contains(&format!("> {selected}")))
        );
    }
    c.master
        .write_all("рд\x1b[200~jkhlqdcnолрдйвст\x1b[201~".as_bytes())
        .unwrap();
    let out = c.read_until(b"scroll");
    screen.feed(&out);
    assert!(screen.row(4).contains("> slot-08"));
    assert!(contains(&out, b"BEGIN-8") && contains(&out, b"END-8"));
    assert!(!contains(&out, b"New:") && !contains(&out, b"Session deleted"));
    assert!(s.log("slot-08").starts_with(b"BEGIN-8"));
    c.master.write_all(b"\x1b[F").unwrap();
    let out = c.read_until(b"scroll");
    screen.feed(&out);
    assert!(screen.row(10).contains("> slot-00"));
    assert!(screen.row(3).contains("slot-07"));
    assert!(!screen.rows().iter().any(|row| row.contains("slot-09")));
    c.master.write_all(b"\x1b[A").unwrap();
    let out = c.read_until(b"scroll");
    screen.feed(&out);
    assert!(screen.row(9).contains("> slot-01"));
    screen.assert_spare_column();
    c.master.write_all(b"\x1b").unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
    assert!(!s.sessions.join("slot-08").exists());
    // Equal recency falls back to names; the prefix timestamp is not activity.
    for n in 0..10 {
        for suffix in ["log", "ended"] {
            File::open(s.sessions.join(format!("slot-{n:02}.{suffix}")))
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
                .unwrap();
        }
    }
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    assert!(screen.row(3).contains("> slot-00") && screen.row(3).contains("[ended]"));
    c.detach();
    c.restored();
}

#[test]
fn picker_restarts_ended_and_stale_sessions_preserving_history() {
    use std::os::unix::net::UnixListener;
    for stale in [false, true] {
        let s = Sessions::new();
        let name = if stale { "stale" } else { "ended" };
        s.ok(&["start", name, "printf", "OLD_BEGINNING\nOLD_ENDING\n"]);
        s.ended(name);
        let old_log = s.log(name);
        let head = s.sessions.join(format!("{name}.head"));
        let old_head = decoded(&head);
        assert!(!old_log.is_empty() && !old_head.is_empty());
        if stale {
            drop(UnixListener::bind(s.sessions.join(name)).unwrap());
        }
        fs::write(s.root.join(".profile"), "printf 'RESTARTED_SHELL\\n'\n").unwrap();
        let mut c = s.attach(&["pick"]);
        let out = c.read_until(b"scroll");
        assert!(contains(&out, if stale { b"[stale]" } else { b"[ended]" }));
        c.master.write_all(b"\r").unwrap();
        c.read_until(b"RESTARTED_SHELL");
        assert!(s.sessions.join(name).exists());
        wait_for(|| !s.sessions.join(format!("{name}.ended")).exists());
        assert!(s.log(name).starts_with(&old_log));
        assert!(decoded(head).starts_with(&old_head));
        c.master.write_all(b"printf 'AFTER_RESTART\\n'\n").unwrap();
        c.read_until(b"AFTER_RESTART");
        wait_for(|| contains(&s.log(name), b"AFTER_RESTART"));
        c.detach();
        c.restored();
    }
}

#[test]
fn picker_restarts_socket_only_stale_with_current_shell_and_directory() {
    let s = Sessions::new();
    drop(UnixListener::bind(s.sessions.join("socket-only")).unwrap());
    let invoking = s.root.join("invoking");
    fs::create_dir(&invoking).unwrap();
    fs::write(
        s.root.join(".profile"),
        "printf 'FRESH_SHELL:%s:%s\\n' \"$0\" \"$PWD\"\n",
    )
    .unwrap();
    let mut c = Sessions::terminal(
        s.command(BINARY)
            .args(["-q", "pick"])
            .env("TERM", "xterm")
            .current_dir(&invoking),
    );
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"[stale]"));
    assert!(contains(&out, b"History unavailable"));
    c.master.write_all(b"\r").unwrap();
    c.read_until(b"FRESH_SHELL");
    let expected = format!(
        "FRESH_SHELL:-bash:{}",
        invoking.canonicalize().unwrap().display()
    );
    wait_for(|| contains(&s.log("socket-only"), b"FRESH_SHELL:"));
    assert!(
        contains(&s.log("socket-only"), expected.as_bytes()),
        "expected {expected}, log {:?}",
        text(&s.log("socket-only"))
    );
    assert!(s.sessions.join("socket-only").exists());
    c.detach();
    c.restored();
}

#[test]
fn picker_restart_keeps_existing_smaller_cap_and_disabled_logging_policy() {
    for cap in ["256", "0"] {
        let s = Sessions::new();
        let old_output = format!("{}OLD_TAIL\n", "a".repeat(100));
        s.ok(&["start", "retained", "printf", "%s", &old_output]);
        s.ended("retained");
        let old_log = s.log("retained");
        let head = s.sessions.join("retained.head");
        let old_head = decoded(&head);
        fs::write(s.root.join(".profile"), "printf 'NEW_OUTPUT'\nsleep 60\n").unwrap();
        let mut c = s.attach(&["-C", cap, "pick"]);
        c.read_until(b"scroll");
        c.master.write_all(b"\r").unwrap();
        c.read_until(b"NEW_OUTPUT");
        if cap == "0" {
            assert_eq!(s.log("retained"), old_log);
            assert_eq!(decoded(head), old_head);
        } else {
            wait_for(|| contains(&s.log("retained"), b"NEW_OUTPUT"));
            assert!(s.log("retained").ends_with(b"NEW_OUTPUT"));
            assert!(decoded(head).starts_with(&old_head));
            assert!(
                fs::metadata(s.sessions.join("retained.log")).unwrap().len()
                    + fs::metadata(s.sessions.join("retained.head"))
                        .unwrap()
                        .len()
                    <= 256
            );
        }
        c.detach();
        c.restored();
    }
}

#[test]
fn concurrent_stale_restarts_bind_only_one_supervisor_without_sidecars() {
    let s = Sessions::new();
    drop(UnixListener::bind(s.sessions.join("stale")).unwrap());
    let directory_lock = File::open(&s.sessions).unwrap();
    os::lock(directory_lock.as_raw_fd()).unwrap();
    let starts = s.root.join("starts");
    let mut children = (0..2)
        .map(|_| {
            Process(
                s.command(BINARY)
                    .args([
                        "start",
                        "stale",
                        "sh",
                        "-c",
                        "printf x >> \"$1\"; printf STALE_RUNNING; sleep 60",
                        "sh",
                        starts.to_str().unwrap(),
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    sleep(Duration::from_millis(100));
    assert!(
        children
            .iter_mut()
            .all(|child| child.0.try_wait().unwrap().is_none())
    );
    drop(directory_lock);
    assert_eq!(
        children
            .iter_mut()
            .map(Process::wait)
            .filter(ExitStatus::success)
            .count(),
        1
    );
    wait_for(|| contains(&s.log("stale"), b"STALE_RUNNING"));
    assert_eq!(fs::read(starts).unwrap(), b"x");
    assert!(contains(&s.ok(&["list"]), b"[running]"));
    assert!(fs::read_dir(&s.sessions).unwrap().all(|entry| {
        matches!(
            entry.unwrap().file_name().to_str().unwrap(),
            "stale" | "stale.log" | "stale.head"
        )
    }));
}

#[test]
fn picker_late_busy_checks_preserve_selection_focus_offsets_and_restore_tty() {
    use std::fmt::Write as _;
    for late_state in [true, false] {
        let s = Sessions::new();
        let path = s.sessions.join("late");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut history = String::new();
        for n in 0..60 {
            write!(history, "late-line-{n:02}\r\n").unwrap();
        }
        for suffix in ["log", "head"] {
            let file = s.sessions.join(format!("late.{suffix}"));
            fs::write(&file, &history).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            File::open(file)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
                .unwrap();
        }
        let other = s.sessions.join("other.log");
        fs::write(&other, b"OTHER").unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
        File::open(other)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + Duration::from_secs(60)),
            )
            .unwrap();
        let peer = std::thread::spawn(move || {
            let (mut socket, query) = protocol_connection(&listener);
            assert_eq!(query[0], CAPABILITIES);
            if late_state {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            socket.write_all(b"FA").unwrap();
            if !late_state {
                let mut request = [0; 10];
                socket.read_exact(&mut request).unwrap();
                assert_eq!(request[0], ATTACH_FREE);
                socket.write_all(b"NO").unwrap();
            }
            let mut extra = [0; 10];
            assert_eq!(
                socket.read(&mut extra).unwrap(),
                0,
                "attach/resize/input after busy"
            );
        });
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        c.master.write_all(b"j\t\x1b[6~").unwrap();
        let before = c.read_until(b"scroll");
        assert!(contains(&before, b"Sessions (2/2)"));
        assert!(
            contains(&before, b"Beginning (13/60)"),
            "frame: {}",
            text(&before)
        );
        assert!(contains(&before, b"Ending (58/60)"));
        c.master.write_all(b"\r").unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"busy with another client"));
        assert!(contains(&out, b"Sessions (2/2)"));
        assert!(contains(&out, "● Beginning (13/60)".as_bytes()));
        assert!(contains(&out, b"Ending (58/60)"));
        peer.join().unwrap();
        assert!(c.child.0.try_wait().unwrap().is_none());
        c.detach();
        c.restored();
    }
}

#[test]
fn simultaneous_picker_protocol_clients_have_atomic_single_admission() {
    let s = Sessions::new();
    s.ok(&[
        "start",
        "atomic",
        "sh",
        "-c",
        "while IFS= read -r line; do printf 'GOT_%s\\n' \"$line\"; done",
    ]);
    let mut probe = UnixStream::connect(s.sessions.join("atomic")).unwrap();
    probe
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    probe
        .write_all(&packet(CAPABILITIES, &[]).repeat(100))
        .unwrap();
    let mut reply = [0; 2];
    probe.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"FA");
    assert!(probe.read(&mut reply).is_err());
    assert!(contains(&s.ok(&["list"]), b"[running]"));
    drop(probe);
    let barrier = Arc::new(Barrier::new(3));
    let peers = (0..2)
        .map(|candidate| {
            let mut socket = UnixStream::connect(s.sessions.join("atomic")).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket.write_all(&packet(CAPABILITIES, &[])).unwrap();
            socket.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"FA");
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let mut request = packet(ATTACH_FREE, &[]).to_vec();
                for chunk in format!("candidate{candidate}\n").as_bytes().chunks(8) {
                    request.extend(packet(0, chunk));
                }
                socket.write_all(&request).unwrap();
                let mut response = [0; 2];
                socket.read_exact(&mut response).unwrap();
                (candidate, socket, response)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut results = peers
        .into_iter()
        .map(|peer| peer.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|(_, _, response)| response == b"OK")
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, _, response)| response == b"NO")
            .count(),
        1
    );
    let (winner, socket, _) = results
        .iter_mut()
        .find(|(_, _, response)| response == b"OK")
        .unwrap();
    let expected = format!("GOT_candidate{winner}\r\n");
    let mut output = Vec::new();
    wait_for(|| {
        let mut bytes = [0; 1024];
        let n = socket.read(&mut bytes).unwrap();
        output.extend_from_slice(&bytes[..n]);
        contains(&output, expected.as_bytes())
    });
    wait_for(|| contains(&s.log("atomic"), expected.as_bytes()));
    assert!(!contains(
        &s.log("atomic"),
        format!("GOT_candidate{}", 1 - *winner).as_bytes()
    ));
    socket
        .write_all(&packet(ATTACH_FREE, &[]).repeat(100))
        .unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    assert!(socket.read(&mut reply).is_err());
}

#[test]
fn picker_legacy_fallback_uses_fresh_socket_and_discards_late_capability_reply() {
    for late_reply in [false, true] {
        let s = Sessions::new();
        let path = s.sessions.join("legacy");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut probe, query) = protocol_connection(&listener);
            assert_eq!(query[0], CAPABILITIES);
            if late_reply {
                sleep(Duration::from_millis(300));
                let _ = probe.write_all(b"FA");
            }
            drop(probe);
            let (mut attached, request) = protocol_connection(&listener);
            assert_eq!(request[0], 1);
            for kind in [3, 4] {
                let mut request = [0; 10];
                attached.read_exact(&mut request).unwrap();
                assert_eq!(request[0], kind);
            }
            attached.write_all(b"LEGACY_OPEN\n").unwrap();
            let mut extra = [0; 10];
            assert_eq!(attached.read(&mut extra).unwrap(), 0);
        });
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        c.master.write_all(b"\r").unwrap();
        let out = c.read_until(b"LEGACY_OPEN");
        assert!(!contains(&out, b"FA"));
        c.detach();
        c.restored();
        peer.join().unwrap();
    }
}

#[test]
fn picker_failed_creation_retains_name_cursor_then_recovers_or_handles_resize_and_signal() {
    for terminate in [false, true] {
        let s = Sessions::new();
        let shell = s.root.join("missing-shell");
        let mut c = Sessions::terminal(
            s.command(BINARY)
                .args(["-q", "pick"])
                .env("TERM", "xterm")
                .env("SHELL", &shell),
        );
        c.read_until(b"scroll");
        c.master.write_all(b"n").unwrap();
        c.read_until(b"scroll");
        c.master.write_all(&[127; 9]).unwrap();
        c.master
            .write_all(b"edited-name\x1b[D\x1b[D\x1b[D\r")
            .unwrap();
        let out = c.read_until(b"scroll");
        picker_reporting_restored(&out);
        picker_reporting_enabled(&out);
        assert!(contains(&out, b"Cannot open session:"));
        assert!(contains(&out, b"New: edited-name"));
        c.master.write_all(b"X").unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"New: edited-nXame"));
        os::set_size(
            c.slave.as_raw_fd(),
            libc::winsize {
                ws_row: 30,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        signal_child(&c.child.0, "-WINCH");
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"New: edited-nXame"));
        let mut screen = screen::Screen::new(80, 30);
        screen.feed(&out);
        screen.assert_spare_column();
        if terminate {
            signal_child(&c.child.0, "-TERM");
            c.read_until(b"\x1b[?1049l");
            assert!(c.child.wait().success());
        } else {
            fs::write(&shell, "#!/bin/sh\nprintf 'RECOVERED_NAME\\n'\nsleep 60\n").unwrap();
            fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
            c.master.write_all(b"\r").unwrap();
            c.read_until(b"RECOVERED_NAME");
            assert!(s.sessions.join("edited-nXame").exists());
            c.detach();
        }
        c.restored();
    }
}

#[test]
fn picker_established_io_error_exits_instead_of_reopening() {
    let s = Sessions::new();
    let path = s.sessions.join("established");
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let (finish, closing) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut socket, query) = protocol_connection(&listener);
        assert_eq!(query[0], CAPABILITIES);
        socket.write_all(b"FA").unwrap();
        let mut request = [0; 10];
        socket.read_exact(&mut request).unwrap();
        assert_eq!(request[0], ATTACH_FREE);
        socket.write_all(b"OK").unwrap();
        for kind in [3, 4] {
            socket.read_exact(&mut request).unwrap();
            assert_eq!(request[0], kind);
        }
        socket.write_all(b"ESTABLISHED\n").unwrap();
        closing.recv_timeout(Duration::from_secs(5)).unwrap();
        socket.shutdown(std::net::Shutdown::Both).unwrap();
    });
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\r").unwrap();
    c.read_until(b"ESTABLISHED");
    signal_child(&c.child.0, "-STOP");
    wait_for(|| stopped(&c.child.0));
    c.master.write_all(b"x").unwrap();
    finish.send(()).unwrap();
    peer.join().unwrap();
    signal_child(&c.child.0, "-CONT");
    let out = c.read_until(b"rtch:");
    assert!(!contains(&out, b"Session picker"));
    assert!(!c.child.wait().success());
    c.restored();
}

#[test]
fn picker_resume_revalidates_and_refuses_newly_attached_client() {
    let s = Sessions::new();
    s.ok(&[
        "start", "resumed", "sh", "-c",
        "printf 'RESUME_READY\\n'; while IFS= read -r line; do printf 'RESUME_%s\\n' \"$line\"; done",
    ]);
    wait_for(|| contains(&s.log("resumed"), b"RESUME_READY"));
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\r").unwrap();
    c.read_until(b"RESUME_READY");
    c.master.write_all(&[26]).unwrap();
    wait_for(|| stopped(&c.child.0));
    wait_for(|| contains(&s.ok(&["list"]), b"[running]"));
    signal_child(&c.child.0, "-CONT");
    let out = c.read_until(b"RESUME_READY");
    assert!(!contains(&out, b"FA") && !contains(&out, b"OK"));
    c.master.write_all(&[26]).unwrap();
    wait_for(|| stopped(&c.child.0));
    wait_for(|| contains(&s.ok(&["list"]), b"[running]"));
    let mut busy = s.attach(&["attach", "resumed"]);
    busy.read_until(b"RESUME_READY");
    wait_for(|| contains(&s.ok(&["list"]), b"[attached]"));
    signal_child(&c.child.0, "-CONT");
    c.read_until(b"busy with another client");
    assert!(!c.child.wait().success());
    c.restored();
    busy.master.write_all(b"still-connected\n").unwrap();
    busy.read_until(b"RESUME_still-connected");
    busy.detach();
    busy.restored();
}

#[test]
fn picker_resume_with_empty_history_ignores_old_socket_readiness() {
    let s = Sessions::new();
    s.ok(&[
        "start",
        "-C",
        "0",
        "empty-resume",
        "sh",
        "-c",
        "printf 'EMPTY_READY\\n'; while IFS= read -r line; do printf 'EMPTY_%s\\n' \"$line\"; done",
    ]);
    let mut c = s.attach(&["-C", "0", "pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\r").unwrap();
    c.read_until(b"EMPTY_READY");
    signal_child(&c.child.0, "-STOP");
    wait_for(|| stopped(&c.child.0));
    s.push("empty-resume", b"old-output\n");
    sleep(Duration::from_millis(100));
    c.master.write_all(&[26]).unwrap();
    signal_child(&c.child.0, "-CONT");
    wait_for(|| stopped(&c.child.0));
    wait_for(|| contains(&s.ok(&["list"]), b"[running]"));
    s.ok(&["clear", "empty-resume"]);
    signal_child(&c.child.0, "-CONT");
    c.master.write_all(b"new-input\n").unwrap();
    c.read_until(b"EMPTY_new-input");
    c.detach();
    c.restored();
}

#[test]
fn picker_restart_failure_stays_open_and_preserves_history() {
    let s = Sessions::new();
    s.ok(&["start", "unsafe", "printf", "OLD_HISTORY"]);
    s.ended("unsafe");
    let old_log = s.log("unsafe");
    let victim = s.root.join("prefix-target");
    fs::write(&victim, b"PRESERVE_TARGET").unwrap();
    fs::remove_file(s.sessions.join("unsafe.head")).unwrap();
    symlink(&victim, s.sessions.join("unsafe.head")).unwrap();
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\r").unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"Cannot open session:"));
    assert!(c.child.0.try_wait().unwrap().is_none());
    assert_eq!(s.log("unsafe"), old_log);
    assert_eq!(fs::read(victim).unwrap(), b"PRESERVE_TARGET");
    assert!(!s.sessions.join("unsafe").exists());
    c.detach();
    c.restored();
}

#[test]
fn picker_creation_validation_cancel_and_child_hook_recursion_guard() {
    let s = Sessions::new();
    fs::create_dir(s.root.join("bin")).unwrap();
    symlink(BINARY, s.root.join("bin/rtch")).unwrap();
    let hook = s.ok(&["shell-init"]);
    let mut profile = hook;
    profile
        .extend_from_slice(b"printf x >> \"$HOME/profile-count\"\nprintf PROFILE_IN_SESSION\\n\n");
    fs::write(s.root.join(".profile"), profile).unwrap();
    s.ok(&["start", "occupied", "true"]);
    s.ended("occupied");
    let mut c = Sessions::terminal(
        s.command(BINARY)
            .arg("-q")
            .arg("pick")
            .env("TERM", "xterm")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", s.root.join("bin").display()),
            ),
    );
    c.read_until(b"scroll");
    c.master.write_all(b"n").unwrap();
    c.read_until(b"New: session-1");
    c.master.write_all(&[127; 9]).unwrap();
    c.master.write_all(b"occupied\r").unwrap();
    c.read_until(b"already occupied");
    c.master.write_all(b"\x1b").unwrap();
    sleep(Duration::from_millis(120));
    for byte in "т".as_bytes() {
        c.master.write_all(&[*byte]).unwrap();
        sleep(Duration::from_millis(10));
    }
    c.read_until(b"New: session-1");
    c.master.write_all("jkhlqdcnолрдйвст\r".as_bytes()).unwrap();
    c.read_until(b"PROFILE_IN_SESSION");
    assert_eq!(fs::read(s.root.join("profile-count")).unwrap(), b"x");
    assert!(s.sessions.join("session-1jkhlqdcnолрдйвст").exists());
    c.detach();
    c.restored();
}

#[test]
fn picker_standard_keys_create_edit_and_cancel_names_without_management() {
    let s = Sessions::new();
    s.ok(&["start", "saved", "printf", "PRESERVED_HISTORY"]);
    s.ended("saved");
    let history = s.log("saved");
    let head = decoded(s.sessions.join("saved.head"));
    let marker = fs::read(s.sessions.join("saved.ended")).unwrap();
    let shell = s.root.join("test-shell");
    fs::write(
        &shell,
        "#!/bin/sh\nprintf CREATED_STANDARD_KEYS\nsleep 60\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let mut c = Sessions::terminal(
        s.command(BINARY)
            .args(["-q", "pick"])
            .env("TERM", "xterm")
            .env("SHELL", &shell),
    );
    c.read_until(b"scroll");
    os::set_size(
        c.slave.as_raw_fd(),
        libc::winsize {
            ws_row: 2,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    signal_child(&c.child.0, "-WINCH");
    c.read_until(b"Esc resize");
    c.master.write_all(b"\x1b[2~").unwrap();
    let out = c.read_until(b"Esc resize");
    assert!(contains(&out, b"Resize to show and confirm"));
    assert!(!s.sessions.join("session-1").exists());
    os::set_size(
        c.slave.as_raw_fd(),
        libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    signal_child(&c.child.0, "-WINCH");
    c.read_until(b"scroll");
    c.master
        .write_all(b"\x1b[200~\x1b[2~\x1b[3~\x1bOQ\x1b[12~\x1b[201~\t")
        .unwrap();
    let out = c.read_until(b"scroll");
    assert!(!contains(&out, b"New:"));
    assert_eq!(s.log("saved"), history);
    c.master.write_all(b"\x1b[").unwrap();
    sleep(Duration::from_millis(100));
    assert!(c.child.0.try_wait().unwrap().is_none());
    assert!(!s.sessions.join("session-1").exists());
    c.master.write_all(b"2~").unwrap();
    c.read_until(b"scroll");
    c.master.write_all(&[127; 9]).unwrap();
    c.master
        .write_all("界e\u{301}👩‍💻jkhlqdcnолрдйвст\x1b[H\x1b[C\x1b[3~\x1b[3~".as_bytes())
        .unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, "New: 界jkhlqdcnолрдйвст".as_bytes()));
    c.master
        .write_all(b"\x1b[2~\x1bOQ\x1b[12~\x1b[1Q\x1b[1;1Q")
        .unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, "New: 界jkhlqdcnолрдйвст".as_bytes()));
    assert_eq!(s.log("saved"), history);
    assert_eq!(decoded(s.sessions.join("saved.head")), head);
    assert_eq!(fs::read(s.sessions.join("saved.ended")).unwrap(), marker);
    c.master
        .write_all(b"\x1b[F\x1b[200~\x1b[2~\x1b[3~\x1bOQ\x1b[12~\x1b[1Q\x1b[1;1Q\x1b[201~")
        .unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(
        &out,
        "New: 界jkhlqdcnолрдйвст[2~[3~OQ[12~[1Q[1;1Q".as_bytes()
    ));
    c.master.write_all(b"\x1b").unwrap();
    let out = c.read_until(b"scroll");
    assert!(!contains(&out, b"New:"));
    assert!(c.child.0.try_wait().unwrap().is_none());
    c.master.write_all(b"\x1b[2~").unwrap();
    c.read_until(b"scroll");
    c.master.write_all(&[127; 9]).unwrap();
    let name = "界jkhlqdcnолрдйвст";
    c.master.write_all(format!("{name}\r").as_bytes()).unwrap();
    c.read_until(b"CREATED_STANDARD_KEYS");
    assert!(s.sessions.join(name).exists());
    assert!(!s.sessions.join("session-1").exists());
    assert_eq!(s.log("saved"), history);
    assert_eq!(fs::read(s.sessions.join("saved.ended")).unwrap(), marker);
    c.detach();
    c.restored();
}

#[test]
fn picker_exit_keys_signal_and_resize_restore_terminal() {
    let s = Sessions::new();
    for key in [
        &b"\x03"[..],
        &b"\x04"[..],
        &b"\x1c"[..],
        &b"\x1b[99;5u"[..],
        &b"\x1b[100;5u"[..],
        &b"\x1b[92;5u"[..],
        &b"\x1b[27;5;99~"[..],
        &b"\x1b[27;5;100~"[..],
        &b"\x1b[27;5;92~"[..],
        &b"\x1b[1089::99;69u"[..],
        &b"\x1b[1074::100;133u"[..],
        &b"\x1b[52;5u"[..],
        &b"\x1b[27;5;52~"[..],
        &b"\x1b[124;6u"[..],
        &b"\x1b[27;6;124~"[..],
        &b"\x1b[27u"[..],
    ] {
        let mut c = s.attach(&["pick"]);
        picker_reporting_enabled(&c.read_until(b"scroll"));
        c.master.write_all(key).unwrap();
        picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
        assert!(c.child.wait().success());
        c.restored();
    }
    for (detach, key) in [
        ("^]", "\x1b[93;5u"),
        ("^@", "\x1b[32;5u"),
        ("^?", "\x1b[63;5u"),
        ("^I", "\x1b[105;5u"),
        ("^K", "\x1b[107;5u"),
        ("^]", "\x1b[27;5;93~"),
        ("^@", "\x1b[27;5;32~"),
        ("^?", "\x1b[27;5;63~"),
        ("^I", "\x1b[27;5;105~"),
    ] {
        let mut c = s.attach(&["-e", detach, "pick"]);
        picker_reporting_enabled(&c.read_until(b"scroll"));
        c.master.write_all(key.as_bytes()).unwrap();
        picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
        assert!(c.child.wait().success());
        c.restored();
    }
    let mut c = s.attach(&["-e", "^I", "pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\t").unwrap();
    picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
    assert!(c.child.wait().success());
    c.restored();
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    os::set_size(
        c.slave.as_raw_fd(),
        libc::winsize {
            ws_row: 6,
            ws_col: 20,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    assert!(
        Command::new("kill")
            .args(["-WINCH", &c.child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let out = c.read_until(b"Esc exit");
    assert!(contains(&out, b"Resize"));
    assert!(
        Command::new("kill")
            .args(["-TERM", &c.child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
    for signal in ["-HUP", "-INT", "-QUIT"] {
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        assert!(
            Command::new("kill")
                .args([signal, &c.child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
        assert!(c.child.wait().success());
        c.restored();
    }
}

#[test]
fn picker_restores_first_reported_xterm_mode_before_exit_and_attach() {
    let s = Sessions::new();
    s.ok(&["start", "live", "sh", "-c", "printf MODE_READY; cat"]);
    wait_for(|| contains(&s.log("live"), b"MODE_READY"));
    for mode in [-1, 0, 1, 2, 3] {
        let restore = if mode == -1 {
            "\x1b[>4n".to_owned()
        } else {
            format!("\x1b[>4;{mode}m")
        };
        for attach in [false, true] {
            let mut c = s.attach(&["pick"]);
            picker_reporting_enabled(&c.read_until(b"scroll"));
            let mut input = format!("\x1b[>4;999m\x1b[>4;{mode}m\x1b[>4;2m").into_bytes();
            input.push(if attach { b'\r' } else { 3 });
            c.master.write_all(&input).unwrap();
            let output = c.read_until(if attach {
                b"MODE_READY"
            } else {
                b"\x1b[?1049l"
            });
            picker_reporting_restored_to(&output, restore.as_bytes());
            if attach {
                c.detach();
            } else {
                assert!(c.child.wait().success());
            }
            c.restored();
        }
    }
}

#[test]
fn login_hook_guards_and_escape_return_to_invoking_shell() {
    let s = Sessions::new();
    fs::create_dir(s.root.join("bin")).unwrap();
    symlink(BINARY, s.root.join("bin/rtch")).unwrap();
    fs::write(s.root.join(".profile"), s.ok(&["shell-init"])).unwrap();
    let out = s.output(
        s.command("bash").arg("-n").arg(s.root.join(".profile")),
        b"",
    );
    assert!(out.status.success());
    let login = || {
        let mut cmd = s.command("/bin/bash");
        cmd.args([
            "--noprofile",
            "--norc",
            "-ilc",
            ". \"$HOME/.profile\"; printf HOOK_DONE",
        ])
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", s.root.join("bin").display()),
        )
        .env("TERM", "xterm")
        .env_remove("RTCH_BYPASS");
        cmd
    };
    for (key, value) in [
        ("RTCH_SESSION", "parent"),
        ("RTCH_BYPASS", "1"),
        ("TERM", "dumb"),
        ("PATH", "/missing/rtch-bin"),
    ] {
        let mut c = Sessions::terminal(login().env(key, value));
        let out = c.read_until(b"HOOK_DONE");
        assert!(!contains(&out, b"\x1b[?1049h"));
        assert!(c.child.wait().success());
        c.restored();
    }
    let mut c = Sessions::terminal(&mut login());
    c.read_until(b"scroll");
    c.master.write_all(b"\x1b").unwrap();
    let out = c.read_until(b"HOOK_DONE");
    assert!(contains(&out, b"\x1b[?1049l"));
    assert!(c.child.wait().success());
    c.restored();
    let out = s.output(&mut login(), b"");
    assert!(out.status.success());
    assert!(contains(&out.stdout, b"HOOK_DONE"));
    assert!(!out.stdout.contains(&27));
    let out = s.output(
        s.command("bash").args([
            "--noprofile",
            "--norc",
            "-lc",
            ". \"$HOME/.profile\"; printf NONINTERACTIVE_DONE",
        ]),
        b"",
    );
    assert!(out.status.success());
    assert!(contains(&out.stdout, b"NONINTERACTIVE_DONE"));
}

#[test]
fn picker_rejects_unsafe_preview_files_without_disclosing_contents() {
    use std::os::unix::fs::PermissionsExt;
    let s = Sessions::new();
    s.ok(&["start", "private", "printf", "SAFE_OUTPUT"]);
    s.ended("private");
    fs::remove_file(s.sessions.join("private.head")).unwrap();
    let victim = s.root.join("victim");
    fs::write(&victim, b"SYMLINK_SECRET").unwrap();
    symlink(&victim, s.sessions.join("private.head")).unwrap();
    fs::write(s.sessions.join("private.log"), b"WORLD_READABLE_SECRET").unwrap();
    fs::set_permissions(
        s.sessions.join("private.log"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"History unavailable"));
    assert!(!contains(&out, b"SYMLINK_SECRET"));
    assert!(!contains(&out, b"WORLD_READABLE_SECRET"));
    c.detach();
    c.restored();
}

#[test]
fn picker_output_error_restores_termios() {
    let s = Sessions::new();
    let (master, slave) = os::pty(
        None,
        &libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let original = os::term(slave.as_raw_fd()).unwrap();
    // A read-only descriptor still identifies a terminal, but screen writes fail.
    let readonly = File::open(format!("/proc/self/fd/{}", slave.as_raw_fd())).unwrap();
    let stdout_flags = os::flags(readonly.as_raw_fd()).unwrap();
    let flag_probe = Some(readonly.try_clone().unwrap());
    let child = s
        .command(BINARY)
        .arg("pick")
        .env("TERM", "xterm")
        .stdin(slave.try_clone().unwrap())
        .stdout(readonly)
        .stderr(slave.try_clone().unwrap())
        .spawn()
        .unwrap();
    let mut c = Client {
        child: Process(child),
        master,
        slave,
        original,
        stdout_flags,
        flag_probe,
    };
    assert!(!c.child.wait().success());
    c.restored();
}

#[test]
fn picker_all_states_and_live_attach() {
    use std::os::unix::net::UnixListener;
    let s = Sessions::new();
    s.ok(&["start", "finished", "true"]);
    s.ended("finished");
    drop(UnixListener::bind(s.sessions.join("stale")).unwrap());
    let pid_file = s.root.join("live.pid");
    s.ok(&[
        "start",
        "live",
        "sh",
        "-c",
        "echo $$ > \"$1\"; printf 'LIVE_READY\\n'; while IFS= read -r line; do printf 'LIVE_%s\\n' \"$line\"; done",
        "sh",
        pid_file.to_str().unwrap(),
    ]);
    wait_for(|| contains(&s.log("live"), b"LIVE_READY"));
    assert!(contains(&s.ok(&["list"]), b"[running]"));
    let pid = fs::read(&pid_file).unwrap();
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    picker_reporting_enabled(&out);
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    assert!(screen.row(3).contains("> live") && screen.row(3).contains("[running]"));
    assert!(
        screen
            .rows()
            .iter()
            .any(|row| row.contains("finished") && row.contains("[ended]"))
    );
    assert!(
        screen
            .rows()
            .iter()
            .any(|row| row.contains("stale") && row.contains("[stale]"))
    );
    c.master.write_all(b"\r").unwrap();
    picker_reporting_restored(&c.read_until(b"LIVE_READY"));
    c.master.write_all(b"picker-input\n").unwrap();
    c.read_until(b"LIVE_picker-input");
    assert_eq!(fs::read(&pid_file).unwrap(), pid);
    c.detach();
    c.restored();

    let mut attached = s.attach(&["attach", "live"]);
    attached.read_until(b"LIVE_READY");
    wait_for(|| contains(&s.ok(&["list"]), b"[attached]"));
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    assert!(
        screen
            .rows()
            .iter()
            .any(|row| row.contains("live") && row.contains("[attached]"))
    );
    c.master.write_all(b"\r").unwrap();
    c.read_until(b"busy with another client");
    assert!(c.child.0.try_wait().unwrap().is_none());
    assert!(attached.child.0.try_wait().unwrap().is_none());
    attached.master.write_all(b"still-connected\n").unwrap();
    attached.read_until(b"LIVE_still-connected");
    assert_eq!(fs::read(&pid_file).unwrap(), pid);
    assert!(contains(&s.ok(&["list"]), b"[attached]"));
    c.detach();
    c.restored();
    // Ordinary named attach still supports another client.
    let mut named = s.attach(&["attach", "live"]);
    named.read_until(b"LIVE_READY");
    named.detach();
    named.restored();
    attached.master.write_all(b"after-named-attach\n").unwrap();
    attached.read_until(b"LIVE_after-named-attach");
    attached.detach();
    attached.restored();
}

#[test]
fn picker_delete_ended_and_stale_refreshes_neighbour_and_keeps_focus() {
    for delete in ["в", "\x1b[3~"] {
        let s = Sessions::new();
        s.ok(&["start", "ended", "printf", "ENDED_HISTORY"]);
        s.ended("ended");
        let stale = s.sessions.join("stale");
        drop(UnixListener::bind(&stale).unwrap());
        let mut c = s.attach(&["pick"]);
        let out = c.read_until(b"scroll");
        let mut screen = screen::Screen::new(80, 24);
        screen.feed(&out);
        let selected = if screen.row(3).contains("> stale") {
            "stale"
        } else {
            "ended"
        };
        let neighbour = if selected == "stale" {
            "ended"
        } else {
            "stale"
        };
        c.master
            .write_all(format!("\t{delete}").as_bytes())
            .unwrap();
        let out = c.read_until(b"scroll");
        screen.feed(&out);
        screen.assert_spare_column();
        assert!(screen.row(3).contains(&format!("> {neighbour}")));
        assert!(screen.row(5).contains("● Beginning"));
        assert!(contains(&out, b"Session deleted"));
        for suffix in ["", ".log", ".head", ".ended"] {
            assert!(!s.sessions.join(format!("{selected}{suffix}")).exists());
        }
        assert!(c.child.0.try_wait().unwrap().is_none());
        c.master.write_all(delete.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"No sessions"));
        assert!(s.ok(&["list"]).starts_with(b"(no sessions)"));
        c.master
            .write_all(format!("{delete}\x1bOQ\x1b").as_bytes())
            .unwrap();
        c.read_until(b"\x1b[?1049l");
        assert!(c.child.wait().success());
        c.restored();
    }
}

#[test]
fn picker_live_delete_refusal_and_clear_preserve_process_and_attachment() {
    for (attached, delete, clear) in [
        (false, "d", "c"),
        (true, "d", "c"),
        (false, "в", "с"),
        (true, "в", "с"),
        (false, "\x1b[3~", "\x1bOQ"),
        (true, "\x1b[3~", "\x1bOQ"),
        (false, "\x1b[3~", "\x1b[12~"),
        (true, "\x1b[3~", "\x1b[12~"),
    ] {
        let s = Sessions::new();
        s.config("log_size = 256\n");
        let pid_file = s.root.join("live.pid");
        s.ok(&[
            "start", "live", "sh", "-c",
            "echo $$ > \"$1\"; printf 'OLD_HISTORY\\n'; while IFS= read -r line; do printf 'NEW_%s\\n' \"$line\"; done",
            "sh", pid_file.to_str().unwrap(),
        ]);
        wait_for(|| contains(&s.log("live"), b"OLD_HISTORY"));
        let pid = fs::read(&pid_file).unwrap();
        let mut live = attached.then(|| {
            let mut c = s.attach(&["attach", "live"]);
            c.read_until(b"OLD_HISTORY");
            c
        });
        let history = s.log("live");
        let head = decoded(s.sessions.join("live.head"));
        let mut picker = s.attach(&["pick"]);
        picker.read_until(b"scroll");
        picker.master.write_all(delete.as_bytes()).unwrap();
        let out = picker.read_until(b"scroll");
        assert!(contains(&out, b"only ended/stale"));
        assert!(contains(
            &out,
            if attached { b"attached" } else { b"running" }
        ));
        assert_eq!(s.log("live"), history);
        assert_eq!(decoded(s.sessions.join("live.head")), head);
        assert_eq!(fs::read(&pid_file).unwrap(), pid);
        assert!(s.sessions.join("live").exists());
        assert!(picker.child.0.try_wait().unwrap().is_none());
        if let Some(c) = &mut live {
            assert!(c.child.0.try_wait().unwrap().is_none());
        }
        picker.master.write_all(clear.as_bytes()).unwrap();
        let out = picker.read_until(b"scroll");
        assert!(contains(&out, b"Logs cleaned"));
        assert_eq!(text(&out).matches("(No output recorded.)").count(), 2);
        assert!(s.log("live").is_empty());
        assert!(decoded(s.sessions.join("live.head")).is_empty());
        assert!(s.sessions.join("live").exists());
        assert_eq!(fs::read(&pid_file).unwrap(), pid);
        if let Some(c) = &mut live {
            c.master.write_all(b"after-clear\n").unwrap();
            c.read_until(b"NEW_after-clear");
        } else {
            s.push("live", b"after-clear\n");
        }
        wait_for(|| contains(&s.log("live"), b"NEW_after-clear"));
        let mut replay = s.attach(&["attach", "live"]);
        let out = replay.read_until(b"NEW_after-clear");
        assert!(!contains(&out, b"OLD_HISTORY"));
        replay.detach();
        replay.restored();
        for _ in 0..20 {
            s.push("live", b"output-to-rotate-the-retained-log\n");
        }
        wait_for(|| contains(&s.log("live"), b"NEW_output-to-rotate"));
        assert!(
            fs::metadata(s.sessions.join("live.log")).unwrap().len()
                + fs::metadata(s.sessions.join("live.head")).unwrap().len()
                <= 256
        );
        assert!(decoded(s.sessions.join("live.head")).starts_with(b"after-clear"));
        picker.master.write_all(b"q").unwrap();
        picker.read_until(b"\x1b[?1049l");
        assert!(picker.child.wait().success());
        picker.restored();
        if let Some(mut c) = live {
            c.detach();
            c.restored();
        }
    }
}

#[test]
fn picker_upward_navigation_and_ambiguous_keys_preserve_live_sessions_in_every_pane() {
    use std::fmt::Write as _;
    let s = Sessions::new();
    s.ok(&["start", "other", "sh", "-c", "printf OTHER_READY; cat"]);
    wait_for(|| contains(&s.log("other"), b"OTHER_READY"));
    let mut history = String::new();
    for i in 0..60 {
        writeln!(history, "line-{i:02}").unwrap();
    }
    s.ok(&[
        "start",
        "live",
        "sh",
        "-c",
        "printf '%s' \"$1\"; cat",
        "sh",
        &history,
    ]);
    wait_for(|| contains(&s.log("live"), b"line-59"));
    let saved = s.log("live");
    for (prepare, position) in [
        ("j", "Sessions (1/2)"),
        ("\tj", "Beginning (1/60)"),
        ("\t\t\x1b[Hj", "Ending (1/60)"),
    ] {
        for key in ["k", "л"] {
            let mut c = s.attach(&["pick"]);
            picker_reporting_enabled(&c.read_until(b"scroll"));
            c.master.write_all(prepare.as_bytes()).unwrap();
            c.read_until(b"scroll");
            c.master.write_all(key.as_bytes()).unwrap();
            let output = c.read_until(b"scroll");
            assert!(contains(&output, position.as_bytes()), "{}", text(&output));
            assert!(!contains(&output, b"Session stopped"));
            c.master
                .write_all(b"K\x0b\x1b[107;5u\x1b[107;6:3u\x1b[27;2;75~")
                .unwrap();
            let output = c.read_until(b"scroll");
            assert!(!contains(&output, b"Session stopped"));
            assert_eq!(s.log("live"), saved);
            assert_eq!(text(&s.ok(&["list"])).matches("[running]").count(), 2);
            c.master.write_all(b"n").unwrap();
            let output = c.read_until(b"scroll");
            assert!(contains(&output, b"New: session-1"));
            for kill in [b"\x1b[107;6u".as_slice(), b"\x1b[27;6;75~".as_slice()] {
                c.master.write_all(kill).unwrap();
                let output = c.read_until(b"scroll");
                assert!(contains(&output, b"New: session-1"));
                assert!(!contains(&output, b"Session stopped"));
                assert!(!s.sessions.join("live.ended").exists());
                assert_eq!(text(&s.ok(&["list"])).matches("[running]").count(), 2);
            }
            c.master.write_all(b"\x1b[91;5uq").unwrap();
            picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
            assert!(c.child.wait().success());
            c.restored();
        }
    }
    s.push("live", b"STILL_ALIVE\n");
    wait_for(|| contains(&s.log("live"), b"STILL_ALIVE"));
}

#[test]
fn picker_enhanced_backtab_preserves_focus_and_reporting() {
    let s = Sessions::new();
    for key in ["\x1b[9;2u", "\x1b[27;2;9~"] {
        let mut c = s.attach(&["pick"]);
        picker_reporting_enabled(&c.read_until(b"scroll"));
        c.master.write_all(b"\t\t").unwrap();
        assert!(contains(&c.read_until(b"scroll"), "● Ending".as_bytes()));
        c.master.write_all(key.as_bytes()).unwrap();
        assert!(contains(&c.read_until(b"scroll"), "● Beginning".as_bytes()));
        c.master.write_all(b"\x1b[27u").unwrap();
        picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
        assert!(c.child.wait().success());
        c.restored();
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn picker_force_stop_terminates_all_jobs_preserves_other_session_and_then_deletes() {
    for (attached, key, focus) in [
        (false, "\x1b[107;6u", ""),
        (false, "\x1b[27;6;75~", "\t"),
        (false, "\x1b[1083;6u", "\t\t"),
        (true, "\x1b[27;6;107~", ""),
        (true, "\x1b[1051;6u", "\t"),
        (true, "\x1b[27;6;1083~", "\t\t"),
    ] {
        let mut s = Sessions::new();
        s.ok(&[
            "start", "other", "sh", "-c",
            "printf 'OTHER_READY\n'; while IFS= read -r line; do printf 'OTHER_%s\n' \"$line\"; done",
        ]);
        wait_for(|| contains(&s.log("other"), b"OTHER_READY"));
        let files = ["shell.pid", "background.pid", "foreground.pid"].map(|name| s.root.join(name));
        s.ok(&[
            "start", "selected", "bash", "--noprofile", "--norc", "-c",
            r#"set -m; trap '' HUP TERM; printf '%s' "$$" > "$1"; sh -c 'trap "" HUP TERM; printf "%s" "$$" > "$1"; exec sleep 60' sh "$2" & sh -c 'trap "" HUP TERM; printf "%s" "$$" > "$1"; printf "STOP_READY\n"; exec sleep 60' sh "$3"; wait"#,
            "bash", files[0].to_str().unwrap(), files[1].to_str().unwrap(), files[2].to_str().unwrap(),
        ]);
        wait_for(|| {
            files
                .iter()
                .all(|path| fs::metadata(path).is_ok_and(|m| m.len() > 0))
        });
        wait_for(|| contains(&s.log("selected"), b"STOP_READY"));
        let jobs = files.map(|path| fs::read_to_string(path).unwrap().parse::<i32>().unwrap());
        let groups = jobs.map(|pid| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let fields = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            assert_eq!(fields[3].parse::<i32>().unwrap(), jobs[0]);
            fields[2].parse::<i32>().unwrap()
        });
        assert_ne!(groups[0], groups[1]);
        assert_ne!(groups[0], groups[2]);
        assert_ne!(groups[1], groups[2]);
        s.track_jobs("selected");
        let history = s.log("selected");
        let head = decoded(s.sessions.join("selected.head"));
        let mut live = attached.then(|| {
            let mut client = s.attach(&["attach", "selected"]);
            client.read_until(b"STOP_READY");
            client
        });
        let mut picker = s.attach(&["pick"]);
        let mut screen = screen::Screen::new(80, 24);
        screen.feed(&picker.read_until(b"scroll"));
        assert!(screen.row(3).contains("> selected"));
        picker
            .master
            .write_all("\x1b[200~kл\x1b[107;6u\x1b[27;6;75~\x1b[201~".as_bytes())
            .unwrap();
        picker.read_until(b"scroll");
        assert!(s.sessions.join("selected").exists());
        for pid in jobs {
            assert!(fs::read_to_string(format!("/proc/{pid}/stat")).is_ok());
        }
        picker
            .master
            .write_all(format!("{focus}{key}").as_bytes())
            .unwrap();
        let out = picker.read_until(b"Session stopped");
        screen.feed(&out);
        // Drain the rest of this frame before the next management key.
        if !contains(&out, b"scroll") {
            screen.feed(&picker.read_until(b"scroll"));
        }
        s.ended("selected");
        wait_for(|| {
            jobs.iter().all(|pid| {
                fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                    matches!(
                        stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
                        Some("Z" | "X")
                    )
                })
            })
        });
        assert!(
            screen
                .rows()
                .iter()
                .any(|row| row.contains("> selected") && row.contains("[ended]"))
        );
        if !focus.is_empty() {
            assert!(
                screen
                    .rows()
                    .iter()
                    .any(|row| row.contains(if focus == "\t" {
                        "● Beginning"
                    } else {
                        "● Ending"
                    }))
            );
        }
        assert!(s.log("selected").starts_with(&history));
        assert!(decoded(s.sessions.join("selected.head")).starts_with(&head));
        assert!(picker.child.0.try_wait().unwrap().is_none());
        if let Some(client) = &mut live {
            assert!(client.child.wait().success());
            client.restored();
        }
        s.push("other", b"after-stop\n");
        wait_for(|| contains(&s.log("other"), b"OTHER_after-stop"));
        picker.master.write_all(b"\x1b[3~").unwrap();
        picker.read_until(b"Session deleted");
        for suffix in ["", ".log", ".head", ".ended"] {
            assert!(!s.sessions.join(format!("selected{suffix}")).exists());
        }
        assert!(picker.child.0.try_wait().unwrap().is_none());
        picker.detach();
        picker.restored();
    }
}

#[test]
fn picker_force_stop_offline_missing_empty_and_printable_detach_preserve_sessions() {
    for state in ["ended", "stale", "missing"] {
        let s = Sessions::new();
        s.ok(&["start", "saved", "printf", "RETAINED"]);
        s.ended("saved");
        if state == "stale" {
            drop(UnixListener::bind(s.sessions.join("saved")).unwrap());
        }
        let history = s.log("saved");
        let head = decoded(s.sessions.join("saved.head"));
        let marker = fs::read(s.sessions.join("saved.ended")).unwrap();
        let mut picker = s.attach(&["pick"]);
        picker.read_until(b"scroll");
        if state == "missing" {
            s.ok(&["rm", "saved"]);
        }
        picker.master.write_all(b"\x1b[1083;6u").unwrap();
        let out = picker.read_until(b"scroll");
        assert!(contains(
            &out,
            if state == "missing" {
                b"Session is missing"
            } else {
                b"already stopped"
            }
        ));
        if state == "missing" {
            assert!(contains(&out, b"No sessions"));
            picker.master.write_all(b"\x1b[107;6u").unwrap();
            let out = picker.read_until(b"scroll");
            assert!(!contains(&out, b"Session stopped"));
        } else {
            assert_eq!(s.log("saved"), history);
            assert_eq!(decoded(s.sessions.join("saved.head")), head);
            assert_eq!(fs::read(s.sessions.join("saved.ended")).unwrap(), marker);
            assert_eq!(s.sessions.join("saved").exists(), state == "stale");
        }
        picker.detach();
        picker.restored();
    }
    let s = Sessions::new();
    s.ok(&["start", "live", "sh", "-c", "printf LIVE_READY; cat"]);
    wait_for(|| contains(&s.log("live"), b"LIVE_READY"));
    let mut picker = s.attach(&["-e", "k", "pick"]);
    picker.read_until(b"scroll");
    picker.master.write_all(b"\x1b[200~k\x1b[201~").unwrap();
    picker.read_until(b"scroll");
    assert!(picker.child.0.try_wait().unwrap().is_none());
    picker.master.write_all(b"k").unwrap();
    assert!(picker.child.wait().success());
    picker.restored();
    assert!(s.sessions.join("live").exists());
    assert!(contains(&s.ok(&["list"]), b"[running]"));
}

#[test]
fn picker_force_stop_refuses_unidentified_peer_without_signaling_or_exiting() {
    let s = Sessions::new();
    let listener = UnixListener::bind(s.sessions.join("unidentified")).unwrap();
    let mut picker = s.attach(&["pick"]);
    picker.read_until(b"scroll");
    picker.master.write_all(b"\x1b[107;6u").unwrap();
    let out = picker.read_until(b"scroll");
    assert!(contains(&out, b"Cannot stop"));
    assert!(s.sessions.join("unidentified").exists());
    assert!(picker.child.0.try_wait().unwrap().is_none());
    picker.detach();
    picker.restored();
    drop(listener);
    fs::remove_file(s.sessions.join("unidentified")).unwrap();
}

#[test]
#[allow(unsafe_code)]
fn captured_session_cleanup_preserves_a_process_that_calls_setsid_after_snapshot() {
    use std::os::fd::FromRawFd;

    struct Escaped(File);
    impl Drop for Escaped {
        fn drop(&mut self) {
            // SAFETY: this owned pidfd identifies only the private escaped job,
            // even if its numeric PID exits and gets reused during cleanup.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
    let mut s = Sessions::new();
    let files = [
        "shell.pid",
        "before.pid",
        "escape.go",
        "after.pid",
        "original.pid",
    ]
    .map(|name| s.root.join(name));
    s.ok(&[
        "start", "scope", "sh", "-c",
        r#"trap '' HUP TERM; printf '%s' "$$" > "$1"; sh -c "$6" sh "$2" "$3" "$4" & sh -c 'trap "" HUP TERM; printf "%s" "$$" > "$1"; exec sleep 60' sh "$5" & wait"#,
        "sh", files[0].to_str().unwrap(), files[1].to_str().unwrap(), files[2].to_str().unwrap(), files[3].to_str().unwrap(), files[4].to_str().unwrap(),
        r#"trap '' HUP TERM; printf '%s' "$$" > "$1"; while [ ! -e "$2" ]; do sleep 0.02; done; exec setsid sh -c 'printf "%s" "$$" > "$1"; exec sleep 60' sh "$3""#,
    ]);
    wait_for(|| {
        [0, 1, 4]
            .iter()
            .all(|&i| fs::metadata(&files[i]).is_ok_and(|m| m.len() > 0))
    });
    let pid = |i: usize| {
        fs::read_to_string(&files[i])
            .unwrap()
            .parse::<i32>()
            .unwrap()
    };
    let original = [pid(0), pid(4)];
    let escaped_pid = pid(1);
    let socket = UnixStream::connect(s.sessions.join("scope")).unwrap();
    let snapshot = os::ChildSession::identify(&socket).unwrap();
    s.track_jobs("scope");
    // SAFETY: escaped_pid is a positive PID read from this private fixture.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, escaped_pid, 0) };
    assert!(descriptor >= 0);
    // SAFETY: pidfd_open returned a new descriptor with unique ownership.
    let escaped = Escaped(unsafe { File::from_raw_fd(i32::try_from(descriptor).unwrap()) });
    fs::write(&files[2], b"go").unwrap();
    wait_for(|| fs::metadata(&files[3]).is_ok_and(|m| m.len() > 0));
    assert_eq!(pid(3), escaped_pid, "setsid must run in the captured PID");
    // SAFETY: getsid only inspects the positive private fixture PID.
    assert_eq!(unsafe { libc::getsid(escaped_pid) }, escaped_pid);
    snapshot.kill().unwrap();
    wait_for(|| {
        original.iter().all(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                matches!(
                    stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
                    Some("Z" | "X")
                )
            })
        })
    });
    s.ended("scope");
    let stat = fs::read_to_string(format!("/proc/{escaped_pid}/stat")).unwrap();
    assert!(!matches!(
        stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
        Some("Z" | "X")
    ));
    drop(escaped);
}

#[test]
fn picker_force_stop_updates_local_row_and_scroll_when_refresh_fails() {
    use std::fmt::Write as _;
    let s = Sessions::new();
    let mut history = String::new();
    for i in 0..60 {
        writeln!(history, "line-{i:02}").unwrap();
    }
    s.ok(&[
        "start",
        "selected",
        "sh",
        "-c",
        "printf '%s' \"$1\"; cat",
        "sh",
        &history,
    ]);
    wait_for(|| contains(&s.log("selected"), b"line-59"));
    let saved = s.log("selected");
    let mut picker = s.attach(&["pick"]);
    picker.read_until(b"scroll");
    picker.master.write_all(b"\t\x1b[6~").unwrap();
    assert!(contains(
        &picker.read_until(b"scroll"),
        b"Beginning (13/60)"
    ));
    fs::write(s.sessions.join("broken"), b"not a socket").unwrap();
    fs::write(s.sessions.join("broken.log"), b"UNRELATED").unwrap();
    picker.master.write_all(b"\x1b[107;6u").unwrap();
    let out = picker.read_until(b"scroll");
    assert!(contains(&out, b"Session stopped"));
    assert!(contains(&out, b"Cannot refresh sessions"));
    assert!(contains(&out, b"Beginning (13/60)") && contains(&out, "● Beginning".as_bytes()));
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    assert!(screen.row(3).contains("> selected") && screen.row(3).contains("[ended]"));
    assert_eq!(s.log("selected"), saved);
    picker.detach();
    picker.restored();
}

#[test]
fn picker_offline_clear_preserves_identity_marker_and_stale_socket() {
    for (stale, clear) in [
        (false, "c"),
        (true, "c"),
        (false, "с"),
        (true, "с"),
        (false, "\x1b[1Q"),
        (true, "\x1b[1;1Q"),
    ] {
        let s = Sessions::new();
        s.ok(&["start", "saved", "printf", "RETAINED"]);
        s.ended("saved");
        let marker = fs::read(s.sessions.join("saved.ended")).unwrap();
        let socket = s.sessions.join("saved");
        if stale {
            drop(UnixListener::bind(&socket).unwrap());
        }
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        c.master
            .write_all(format!("\t\t{clear}").as_bytes())
            .unwrap();
        let out = c.read_until(b"scroll");
        let mut screen = screen::Screen::new(80, 24);
        screen.feed(&out);
        screen.assert_spare_column();
        assert!(screen.row(3).contains("> saved"));
        assert!(screen.row(8).contains("● Ending"));
        assert_eq!(text(&out).matches("(No output recorded.)").count(), 2);
        assert!(s.log("saved").is_empty());
        assert!(decoded(s.sessions.join("saved.head")).is_empty());
        assert_eq!(fs::read(s.sessions.join("saved.ended")).unwrap(), marker);
        assert_eq!(socket.exists(), stale);
        assert!(contains(
            &s.ok(&["list"]),
            if stale { b"[stale]" } else { b"[ended]" }
        ));
        c.master.write_all(b"q").unwrap();
        c.read_until(b"\x1b[?1049l");
        assert!(c.child.wait().success());
        c.restored();
    }
}

#[test]
fn picker_actions_revalidate_changed_and_missing_selections() {
    let s = Sessions::new();
    s.ok(&["start", "changed", "printf", "OLD"]);
    s.ended("changed");
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    s.ok(&["start", "changed", "sh", "-c", "printf LIVE; cat"]);
    wait_for(|| contains(&s.log("changed"), b"LIVE"));
    let old = s.log("changed");
    c.master.write_all(b"\x1b[3~").unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"Session is running"));
    assert_eq!(s.log("changed"), old);
    c.master.write_all(b"\x1bOQ").unwrap();
    c.read_until(b"scroll");
    assert!(s.log("changed").is_empty());
    s.ok(&["kill", "-f", "changed"]);
    s.ended("changed");
    s.ok(&["rm", "changed"]);
    c.master.write_all(b"\x1b[3~").unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"Session is missing"));
    assert!(contains(&out, b"No sessions"));
    assert!(c.child.0.try_wait().unwrap().is_none());
    c.master.write_all(b"q").unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn picker_clear_validates_both_files_and_preserves_focus_and_selection() {
    let s = Sessions::new();
    for name in ["other", "selected"] {
        s.ok(&["start", name, "printf", "SAVED_HISTORY"]);
        s.ended(name);
    }
    let other = s.log("other");
    let victim = s.root.join("victim");
    fs::write(&victim, b"PRESERVE_TARGET").unwrap();
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    fs::remove_file(s.sessions.join("selected.head")).unwrap();
    symlink(&victim, s.sessions.join("selected.head")).unwrap();
    c.master.write_all(b"\tc").unwrap();
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    screen.assert_spare_column();
    assert!(screen.row(3).contains("> selected"));
    assert!(screen.row(5).contains("● Beginning"));
    assert!(contains(&out, b"Cannot clean"));
    assert!(contains(&out, b"History unavailable"));
    assert!(contains(&out, b"SAVED_HISTORY"));
    assert_eq!(s.log("selected"), b"SAVED_HISTORY");
    assert_eq!(s.log("other"), other);
    assert_eq!(fs::read(victim).unwrap(), b"PRESERVE_TARGET");
    c.master.write_all(b"q").unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn picker_busy_directory_keeps_files_and_allows_exit_signal_and_retry() {
    for (action, clean) in [
        ("c", true),
        ("d", false),
        ("\x1b[107;6u", false),
        ("\x1b[27;6;75~", false),
        ("\x1bOQ", true),
        ("\x1b[3~", false),
    ] {
        let s = Sessions::new();
        let stop = matches!(action, "\x1b[107;6u" | "\x1b[27;6;75~");
        if stop {
            s.ok(&["start", "locked", "sh", "-c", "printf LOCKED_HISTORY; cat"]);
            wait_for(|| s.log("locked") == b"LOCKED_HISTORY");
        } else {
            s.ok(&["start", "locked", "printf", "LOCKED_HISTORY"]);
            s.ended("locked");
        }
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        let directory = File::open(&s.sessions).unwrap();
        os::lock(directory.as_raw_fd()).unwrap();
        c.master.write_all(action.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"directory is busy; retry"));
        assert_eq!(s.log("locked"), b"LOCKED_HISTORY");
        assert_eq!(s.sessions.join("locked.ended").exists(), !stop);
        c.master.write_all(b"q").unwrap();
        c.read_until(b"\x1b[?1049l");
        assert!(c.child.wait().success());
        c.restored();
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        c.master.write_all(action.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"directory is busy; retry"));
        signal_child(&c.child.0, "-TERM");
        c.read_until(b"\x1b[?1049l");
        assert!(c.child.wait().success());
        c.restored();
        let mut c = s.attach(&["pick"]);
        c.read_until(b"scroll");
        c.master.write_all(action.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"directory is busy; retry"));
        assert_eq!(s.log("locked"), b"LOCKED_HISTORY");
        // A parallel spawn may hold an inherited copy until exec closes it.
        #[allow(unsafe_code)]
        // SAFETY: directory owns this live descriptor and the test acquired its flock.
        let unlocked = unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_UN) };
        assert_eq!(unlocked, 0);
        drop(directory);
        assert_eq!(s.log("locked"), b"LOCKED_HISTORY");
        c.master.write_all(action.as_bytes()).unwrap();
        c.read_until(if stop {
            b"Session stopped"
        } else if clean {
            b"Logs cleaned"
        } else {
            b"Session deleted"
        });
        c.master.write_all(b"q").unwrap();
        c.read_until(b"\x1b[?1049l");
        assert!(c.child.wait().success());
        c.restored();
    }
}

#[test]
fn picker_refresh_and_actions_keep_the_directory_captured_before_config_changes() {
    let original = Sessions::new();
    let other = Sessions::new();
    for (suite, name, history) in [
        (&original, "original-only", "ORIGINAL_ONLY"),
        (&original, "same", "ORIGINAL_SAME"),
        (&other, "foreign-only", "FOREIGN_ONLY"),
        (&other, "same", "FOREIGN_SAME"),
    ] {
        suite.ok(&["start", name, "printf", history]);
        suite.ended(name);
    }
    let mut c = original.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"ORIGINAL_SAME"));
    fs::write(
        original.root.join("config/rtch/config"),
        format!("session_dir = {}\n", other.sessions.display()),
    )
    .unwrap();
    assert!(contains(&original.ok(&["list"]), b"foreign-only"));
    c.master.write_all(b"c").unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"original-only"));
    assert!(!contains(&out, b"foreign-only"));
    assert!(original.log("same").is_empty());
    assert_eq!(other.log("same"), b"FOREIGN_SAME");
    c.master.write_all(b"d").unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"> original-only"));
    assert!(contains(&out, b"ORIGINAL_ONLY"));
    assert!(!original.sessions.join("same.ended").exists());
    assert!(other.sessions.join("same.ended").exists());
    c.master.write_all(b"c").unwrap();
    c.read_until(b"scroll");
    assert!(original.log("original-only").is_empty());
    assert_eq!(other.log("foreign-only"), b"FOREIGN_ONLY");
    c.master.write_all(b"q").unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn picker_successful_delete_selects_local_neighbour_when_rescan_fails() {
    let s = Sessions::new();
    for (name, history) in [("neighbour", "NEIGHBOUR_HISTORY"), ("deleted", "DELETE_ME")] {
        s.ok(&["start", name, "printf", history]);
        s.ended(name);
    }
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"> deleted"));
    // A malformed unrelated session makes the subsequent rescan fail.
    fs::write(s.sessions.join("broken"), b"not a socket").unwrap();
    fs::write(s.sessions.join("broken.log"), b"UNRELATED").unwrap();
    c.master.write_all(b"\td").unwrap();
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    screen.assert_spare_column();
    assert!(screen.row(3).contains("> neighbour"));
    assert!(screen.row(5).contains("● Beginning"));
    assert!(contains(&out, b"NEIGHBOUR_HISTORY"));
    assert!(contains(&out, b"Cannot refresh sessions"));
    for suffix in ["", ".log", ".head", ".ended"] {
        assert!(!s.sessions.join(format!("deleted{suffix}")).exists());
    }
    assert_eq!(s.log("neighbour"), b"NEIGHBOUR_HISTORY");
    c.master.write_all(b"q").unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn picker_hidden_selection_requires_resize_before_stop_delete_or_clean() {
    let s = Sessions::new();
    s.ok(&["start", "hidden", "printf", "PRESERVED_HISTORY"]);
    s.ended("hidden");
    let log = s.log("hidden");
    let head = decoded(s.sessions.join("hidden.head"));
    let marker = fs::read(s.sessions.join("hidden.ended")).unwrap();
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    for (width, height) in [(80, 5), (20, 24)] {
        os::set_size(
            c.slave.as_raw_fd(),
            libc::winsize {
                ws_row: height,
                ws_col: width,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        signal_child(&c.child.0, "-WINCH");
        c.read_until(if width == 20 { b"Esc exit" } else { b"scroll" });
        for action in [
            "d",
            "c",
            "\x1b[107;6u",
            "в",
            "с",
            "\x1b[27;6;75~",
            "\x1b[3~",
            "\x1bOQ",
            "\x1b[12~",
        ] {
            c.master.write_all(action.as_bytes()).unwrap();
            let out = c.read_until(if width == 20 { b"Esc exit" } else { b"scroll" });
            assert!(contains(&out, b"Resize before"));
            assert_eq!(s.log("hidden"), log);
            assert_eq!(decoded(s.sessions.join("hidden.head")), head);
            assert_eq!(fs::read(s.sessions.join("hidden.ended")).unwrap(), marker);
        }
    }
    c.master.write_all("олр дй".as_bytes()).unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn picker_retained_long_log_scrolls_both_previews_and_resizes_with_stable_borders() {
    let s = Sessions::new();
    let line = format!("BEGIN{}SUFFIX", "0123456789".repeat(24));
    s.ok(&["start", "long", "printf", "%s", &line]);
    s.ended("long");
    assert_eq!(s.log("long"), line.as_bytes());
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    screen.assert_spare_column();
    assert!(screen.row(12).contains("Beginning (1/1)"));
    assert!(screen.row(17).contains("Ending (1/1)"));
    assert!(!contains(&out, b"SUFFIX"));
    let mut keys = vec![b'\t'];
    keys.extend("д".repeat(200).as_bytes());
    c.master.write_all(&keys).unwrap();
    let marker = b"col 177/251";
    let mut out = c.read_until(marker);
    let marker_at = out
        .windows(marker.len())
        .rposition(|bytes| bytes == marker)
        .unwrap();
    // Ignore resets from earlier redraws; this column's frame must be complete.
    if !contains(&out[marker_at..], b"\x1b[0m") {
        out.extend(c.read_until(b"\x1b[0m"));
    }
    let frame_end = marker_at
        + out[marker_at..]
            .windows(4)
            .position(|bytes| bytes == b"\x1b[0m")
            .unwrap()
        + 4;
    screen.feed(&out[..frame_end]);
    screen.assert_spare_column();
    assert!(screen.row(5).contains("col 177/251"));
    assert!(screen.row(6).contains(&line[176..]));
    assert!(!screen.row(7).contains("SUFFIX"));
    let mut keys = vec![b'\t'];
    keys.extend(b"\x1b[C".repeat(15));
    c.master.write_all(&keys).unwrap();
    let marker = b"col 16/251";
    let mut out = c.read_until(marker);
    let marker_at = out
        .windows(marker.len())
        .rposition(|bytes| bytes == marker)
        .unwrap();
    if !contains(&out[marker_at..], b"\x1b[0m") {
        out.extend(c.read_until(b"\x1b[0m"));
    }
    let frame_end = marker_at
        + out[marker_at..]
            .windows(4)
            .position(|bytes| bytes == b"\x1b[0m")
            .unwrap()
        + 4;
    screen.feed(&out[..frame_end]);
    screen.assert_spare_column();
    assert!(screen.row(5).contains("col 177/251"));
    assert!(screen.row(6).contains(&line[176..]));
    assert!(screen.row(8).contains("col 16/251"));
    assert!(screen.row(9).contains(&line[15..90]));
    os::set_size(
        c.slave.as_raw_fd(),
        libc::winsize {
            ws_row: 95,
            ws_col: 196,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    signal_child(&c.child.0, "-WINCH");
    let marker = b"col 61/251";
    let mut out = c.read_until(marker);
    let marker_at = out
        .windows(marker.len())
        .rposition(|bytes| bytes == marker)
        .unwrap();
    if !contains(&out[marker_at..], b"\x1b[0m") {
        out.extend(c.read_until(b"\x1b[0m"));
    }
    let frame_end = marker_at
        + out[marker_at..]
            .windows(4)
            .position(|bytes| bytes == b"\x1b[0m")
            .unwrap()
        + 4;
    let mut screen = screen::Screen::new(196, 95);
    screen.feed(&out[..frame_end]);
    screen.assert_spare_column();
    assert!(screen.row(5).contains("Beginning (1/1) · col 61/251"));
    assert!(screen.row(6).contains(&line[60..]));
    assert!(screen.row(8).contains("Ending (1/1) · col 16/251"));
    assert!(screen.row(9).contains(&line[15..206]));
    for row in [3, 6, 9, 91] {
        assert!(screen.row(row).starts_with('│') && screen.row(row).ends_with("│ "));
    }
    c.master.write_all("рй".as_bytes()).unwrap();
    c.read_until(b"\x1b[?1049l");
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn picker_control_exit_bytes_still_restore_terminal_inside_bracketed_paste() {
    let s = Sessions::new();
    for key in [3, 4, 28, 29] {
        let args = if key == 29 {
            &["-e", "^]", "pick"][..]
        } else {
            &["pick"][..]
        };
        let mut c = s.attach(args);
        c.read_until(b"scroll");
        let mut bytes = "\x1b[200~jkhlqdcnолрдйвст".as_bytes().to_vec();
        bytes.push(key);
        c.master.write_all(&bytes).unwrap();
        picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
        assert!(c.child.wait().success());
        c.restored();
    }
    for (detach, key) in [
        ("^\\", "\x1b[99;5u"),
        ("^\\", "\x1b[100;133u"),
        ("^\\", "\x1b[92;5u"),
        ("^]", "\x1b[93;5u"),
        ("^\\", "\x1b[27;5;99~"),
        ("^\\", "\x1b[27;5;100~"),
        ("^\\", "\x1b[27;5;92~"),
        ("^]", "\x1b[27;5;93~"),
    ] {
        let mut c = s.attach(&["-e", detach, "pick"]);
        c.read_until(b"scroll");
        c.master.write_all(b"\x1b[200~pasted").unwrap();
        for byte in key.as_bytes() {
            c.master.write_all(&[*byte]).unwrap();
        }
        picker_reporting_restored(&c.read_until(b"\x1b[?1049l"));
        assert!(c.child.wait().success());
        c.restored();
    }
}

#[test]
fn picker_attached_shortcuts_and_standard_key_bytes_reach_the_program_unchanged() {
    let s = Sessions::new();
    let input_file = s.root.join("input");
    let input = "jkhlqdcnолрдйвст\x1b[A\x1b[B\x1b[D\x1b[C\x1b[2~\x1b[3~\x1bOQ\x1b[12~\x1b[1Q\x1b[1;1Q\t\x1b[Z\x1b[5~\x1b[6~\x1b[H\x1b[F\n".as_bytes();
    let input_len = input.len().to_string();
    s.ok(&[
        "start", "literal", "sh", "-c",
        "stty raw -echo; printf INPUT_READY; dd bs=1 count=\"$2\" of=\"$1\" 2>/dev/null; printf INPUT_SAVED; sleep 30",
        "sh", input_file.to_str().unwrap(), &input_len,
    ]);
    wait_for(|| contains(&s.log("literal"), b"INPUT_READY"));
    let mut c = s.attach(&["pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\r").unwrap();
    c.read_until(b"INPUT_READY");
    c.master.write_all(input).unwrap();
    c.read_until(b"INPUT_SAVED");
    assert_eq!(fs::read(input_file).unwrap(), input);
    assert!(s.sessions.join("literal").exists());
    c.detach();
    c.restored();
}

#[test]
fn picker_selection_resets_scrolled_previews_and_reports_empty_missing_history() {
    use std::fmt::Write as _;
    let s = Sessions::new();
    for name in ["alpha", "beta"] {
        let mut history = String::new();
        for line in 0..60 {
            writeln!(history, "{name}-line-{line:02}").unwrap();
        }
        s.ok(&["start", name, "printf", "%s", &history]);
        s.ended(name);
    }
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    assert!(screen.row(3).contains("> beta") && screen.row(3).contains("[ended]"));
    c.master.write_all(b"\t\x1b[6~").unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"beta-line-12"));
    for (down, up) in [("j", "\x1b[A"), ("о", "\x1b[A")] {
        c.master.write_all(down.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"Beginning (14/60)"));
        assert!(contains(&out, b"beta-line-13"));
        c.master.write_all(up.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"Beginning (13/60)"));
    }
    c.master.write_all(b"\t\x1b[H").unwrap();
    c.read_until(b"scroll");
    for (down, up) in [("j", "\x1b[A"), ("о", "\x1b[A")] {
        c.master.write_all(down.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"Ending (2/60)"));
        assert!(contains(&out, b"Beginning (13/60)"));
        c.master.write_all(up.as_bytes()).unwrap();
        let out = c.read_until(b"scroll");
        assert!(contains(&out, b"Ending (1/60)"));
    }
    c.master.write_all("\x1b[Z\x1b[Zо".as_bytes()).unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"Beginning (1/60)"));
    assert!(contains(&out, b"Ending (58/60)"));
    assert!(contains(&out, b"alpha-line-00"));
    c.detach();
    c.restored();

    s.ok(&["clear", "alpha"]);
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    assert_eq!(text(&out).matches("(No output recorded.)").count(), 2);
    c.detach();

    fs::remove_file(s.sessions.join("alpha.log")).unwrap();
    fs::remove_file(s.sessions.join("alpha.head")).unwrap();
    File::options()
        .write(true)
        .open(s.sessions.join("alpha.ended"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .unwrap();
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    assert_eq!(text(&out).matches("History unavailable").count(), 2);
    c.detach();
    c.restored();
}

#[test]
fn login_hook_failure_survives_interactive_set_e_profile() {
    use std::os::unix::fs::PermissionsExt;
    let s = Sessions::new();
    fs::create_dir(s.root.join("bin")).unwrap();
    let fake = s.root.join("bin/rtch");
    fs::write(&fake, b"#!/bin/sh\nexit 7\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let mut profile = b"set -e\n".to_vec();
    profile.extend(s.ok(&["shell-init"]));
    profile.extend_from_slice(b"printf PROFILE_USABLE\n");
    fs::write(s.root.join(".profile"), profile).unwrap();
    let mut c = Sessions::terminal(
        s.command("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-ilc",
                ". \"$HOME/.profile\"; printf SHELL_USABLE",
            ])
            .env("TERM", "xterm")
            .env_remove("RTCH_BYPASS")
            .env("PATH", s.root.join("bin")),
    );
    let out = c.read_until(b"SHELL_USABLE");
    assert!(contains(&out, b"PROFILE_USABLE"));
    assert!(c.child.wait().success());
    c.restored();
}

#[test]
fn head_only_session_discovery_and_remove_all() {
    use std::os::unix::fs::OpenOptionsExt;
    let s = Sessions::new();
    File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(s.sessions.join("head-only.head"))
        .unwrap()
        .write_all(b"BEGINNING_ONLY\n")
        .unwrap();
    let list = s.ok(&["list"]);
    assert!(contains(&list, b"head-only") && contains(&list, b"[ended]"));
    assert_eq!(text(&list).matches("head-only").count(), 1);
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"BEGINNING_ONLY"));
    assert!(contains(&out, b"History unavailable"));
    c.detach();
    c.restored();
    s.ok(&["rm", "-a"]);
    assert!(!s.sessions.join("head-only.head").exists());
}

#[test]
fn valid_multiline_control_history_is_sanitized_before_wrapping() {
    let s = Sessions::new();
    s.ok(&["start", "controls", "true"]);
    s.ended("controls");
    let history = b"TOP_VISIBLE\n\x1b]0;OSC_SECRET\nSECOND_SECRET\x07MIDDLE_VISIBLE\n\x90DCS_SECRET\nLAST_SECRET\x9cBOTTOM_VISIBLE\n";
    for suffix in ["log", "head"] {
        fs::write(s.sessions.join(format!("controls.{suffix}")), history).unwrap();
    }
    let mut c = s.attach(&["pick"]);
    let out = c.read_until(b"scroll");
    let mut screen = screen::Screen::new(80, 24);
    screen.feed(&out);
    screen.assert_spare_column();
    let rows = screen.rows().join("\n");
    assert!(
        rows.contains("TOP_VISIBLE")
            && rows.contains("MIDDLE_VISIBLE")
            && rows.contains("BOTTOM_VISIBLE")
    );
    assert!(!rows.contains("SECRET"));
    c.detach();
    c.restored();
}

#[test]
fn picker_revalidates_changed_states_at_enter() {
    let s = Sessions::new();
    s.ok(&["start", "changed", "true"]);
    s.ended("changed");
    let mut picker = s.attach(&["pick"]);
    picker.read_until(b"scroll");
    let mut live = s.attach(&[
        "new",
        "changed",
        "sh",
        "-c",
        "printf 'NEW_LIVE\\n'; while IFS= read -r line; do printf 'LIVE_%s\\n' \"$line\"; done",
    ]);
    live.read_until(b"NEW_LIVE");
    wait_for(|| contains(&s.ok(&["list"]), b"[attached]"));
    picker.master.write_all(b"\r").unwrap();
    picker.read_until(b"busy with another client");
    assert!(picker.child.0.try_wait().unwrap().is_none());
    live.master.write_all(b"after-busy\n").unwrap();
    live.read_until(b"LIVE_after-busy");
    picker.detach();
    picker.restored();
    live.detach();
    live.restored();
    let mut picker = s.attach(&["pick"]);
    picker.read_until(b"scroll");
    s.ok(&["kill", "-f", "changed"]);
    s.ended("changed");
    let old_log = s.log("changed");
    let head = s.sessions.join("changed.head");
    let old_head = decoded(&head);
    fs::write(s.root.join(".profile"), "printf 'CHANGED_RESTARTED\\n'\n").unwrap();
    picker.master.write_all(b"\r").unwrap();
    picker.read_until(b"CHANGED_RESTARTED");
    assert!(s.sessions.join("changed").exists());
    assert!(s.log("changed").starts_with(&old_log));
    assert!(decoded(head).starts_with(&old_head));
    picker.detach();
    picker.restored();

    let mut picker = s.attach(&["pick"]);
    picker.read_until(b"scroll");
    s.ok(&["kill", "-f", "changed"]);
    s.ended("changed");
    s.ok(&["rm", "changed"]);
    picker.master.write_all(b"\r").unwrap();
    picker.read_until(b"Session is missing");
    assert!(picker.child.0.try_wait().unwrap().is_none());
    assert!(!s.sessions.join("changed").exists());
    picker.detach();
    picker.restored();
}

#[test]
fn exclusive_picker_creation_preserves_occupied_history_and_stale_socket() {
    use std::os::unix::net::UnixListener;
    let s = Sessions::new();
    s.ok(&["start", "occupied", "printf", "OLD_HISTORY"]);
    s.ended("occupied");
    let history = s.log("occupied");
    assert!(
        !s.run(
            &["__serve", "--exclusive", "occupied", "printf", "RESTARTED"],
            b""
        )
        .status
        .success()
    );
    assert_eq!(s.log("occupied"), history);
    assert!(s.sessions.join("occupied.ended").exists());
    drop(UnixListener::bind(s.sessions.join("stale")).unwrap());
    assert!(
        !s.run(&["__serve", "--exclusive", "stale", "true"], b"")
            .status
            .success()
    );
    assert!(s.sessions.join("stale").exists());
}

#[test]
fn printable_picker_detach_does_not_match_navigation_or_paste_sequences() {
    let s = Sessions::new();
    let mut c = s.attach(&["-e", "A", "pick"]);
    c.read_until(b"scroll");
    c.master.write_all(b"\x1b[A").unwrap();
    c.read_until(b"scroll");
    assert!(c.child.0.try_wait().unwrap().is_none());
    c.master
        .write_all(b"n\x1b[200~A\x1b[H\x1b!\x1b[201~")
        .unwrap();
    let out = c.read_until(b"scroll");
    assert!(contains(&out, b"New:"));
    assert!(c.child.0.try_wait().unwrap().is_none());
    c.master.write_all(b"A").unwrap();
    assert!(c.child.wait().success());
    c.restored();
    assert!(fs::read_dir(&s.sessions).unwrap().next().is_none());
}

#[test]
fn legacy_head_sessions_keep_socket_priority_access_and_safe_removal() {
    use std::os::unix::{fs::OpenOptionsExt, net::UnixListener};
    let s = Sessions::new();
    let legacy = s.sessions.join("foo.head");
    let live = UnixListener::bind(&legacy).unwrap();
    File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(s.sessions.join("foo.ended"))
        .unwrap();
    let list = s.ok(&["list"]);
    assert!(contains(&list, b"foo.head"));
    s.ok(&["rm", "foo"]);
    assert!(
        fs::symlink_metadata(&legacy)
            .unwrap()
            .file_type()
            .is_socket()
    );
    let target = s.root.join("preserve-target");
    fs::write(&target, b"PRESERVE").unwrap();
    let unsafe_head = s.sessions.join("unsafe.head");
    symlink(&target, &unsafe_head).unwrap();
    s.ok(&["rm", "-a"]);
    assert!(fs::symlink_metadata(unsafe_head).is_err());
    assert_eq!(fs::read(target).unwrap(), b"PRESERVE");
    assert!(legacy.exists());
    assert!(!s.run(&["start", "foo", "true"], b"").status.success());
    assert!(!s.sessions.join("foo.log").exists());
    drop(live);
    assert!(contains(&s.ok(&["list"]), b"[stale]"));
    s.ok(&["rm", "foo.head"]);
    assert!(!legacy.exists());
    let log = s.sessions.join("foo.head.log");
    File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log)
        .unwrap()
        .write_all(b"LEGACY_HISTORY\n")
        .unwrap();
    assert!(contains(&s.ok(&["tail", "foo.head"]), b"LEGACY_HISTORY"));
    let list = s.ok(&["list"]);
    assert!(contains(&list, b"foo.head") && contains(&list, b"[ended]"));
    assert!(!s.run(&["start", "foo", "true"], b"").status.success());
    assert_eq!(fs::read(&log).unwrap(), b"LEGACY_HISTORY\n");
    s.ok(&["rm", "foo.head"]);
    assert!(!log.exists());
    assert!(
        !s.run(&["start", "fresh.head", "true"], b"")
            .status
            .success()
    );
}

#[test]
fn compressed_tail_follow_streams_rotation_clear_restart_and_legacy_migration_once() {
    use std::os::unix::fs::OpenOptionsExt;
    let s = Sessions::new();
    let log_path = s.sessions.join("follow.log");
    let mut legacy = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)
        .unwrap();
    legacy.write_all(b"LEGACY_SEEN\n").unwrap();
    let output = s.root.join("follow.out");
    let mut follower = Process(
        s.command(BINARY)
            .args(["tail", "-f", "-n", "1", "follow"])
            .stdout(File::create(&output).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(|| contains(&fs::read(&output).unwrap(), b"LEGACY_SEEN"));
    legacy.write_all(b"LEGACY_LIVE\n").unwrap();
    wait_for(|| contains(&fs::read(&output).unwrap(), b"LEGACY_LIVE"));
    drop(legacy);
    s.ok(&[
        "start",
        "-C",
        "1k",
        "follow",
        "sh",
        "-c",
        "stty -echo; printf 'COMPRESSED_READY\\n'; while IFS= read -r line; do printf '%s\\n' \"$line\"; done",
    ]);
    wait_for(|| contains(&fs::read(&output).unwrap(), b"COMPRESSED_READY"));
    let original_inode = fs::metadata(&log_path).unwrap().ino();
    for i in 0..30 {
        let message = format!("unique followed message {i:03}\n");
        s.push("follow", message.as_bytes());
        wait_for(|| contains(&fs::read(&output).unwrap(), message.trim_end().as_bytes()));
        assert!(
            fs::metadata(&log_path).unwrap().len()
                + fs::metadata(s.sessions.join("follow.head")).unwrap().len()
                <= 1024
        );
    }
    assert_ne!(fs::metadata(&log_path).unwrap().ino(), original_inode);
    s.ok(&["clear", "follow"]);
    s.push("follow", b"AFTER_CLEAR_ONCE\n");
    wait_for(|| contains(&fs::read(&output).unwrap(), b"AFTER_CLEAR_ONCE"));
    s.ok(&["kill", "-f", "follow"]);
    s.ended("follow");
    s.ok(&[
        "start",
        "-C",
        "1k",
        "follow",
        "printf",
        "AFTER_RESTART_ONCE\n",
    ]);
    s.ended("follow");
    wait_for(|| contains(&fs::read(&output).unwrap(), b"AFTER_RESTART_ONCE"));
    signal_child(&follower.0, "-TERM");
    assert!(follower.wait().success());
    let bytes = fs::read(output).unwrap();
    let lines = String::from_utf8(bytes).unwrap();
    for expected in [
        "LEGACY_SEEN",
        "LEGACY_LIVE",
        "COMPRESSED_READY",
        "AFTER_CLEAR_ONCE",
        "AFTER_RESTART_ONCE",
    ] {
        assert_eq!(lines.matches(expected).count(), 1, "{lines}");
    }
    for i in 0..30 {
        assert_eq!(
            lines
                .matches(&format!("unique followed message {i:03}"))
                .count(),
            1
        );
    }
}

#[test]
fn compressed_tail_expands_only_when_needed_and_reads_legacy_without_migration() {
    use std::{fmt::Write as _, os::unix::fs::OpenOptionsExt};
    let s = Sessions::new();
    let mut history = String::new();
    for i in 0..20_000 {
        writeln!(history, "number {i:05} some padded history text").unwrap();
    }
    let source = s.root.join("history.fixture");
    fs::write(&source, &history).unwrap();
    s.ok(&["start", "long-tail", "cat", source.to_str().unwrap()]);
    s.ended("long-tail");
    let tail = s.ok(&["tail", "-n", "3", "long-tail"]);
    let mut expected = String::new();
    for line in history.lines().skip(19_997) {
        write!(expected, "{line}\r\n").unwrap();
    }
    assert_eq!(tail, expected.as_bytes());
    let larger = s.ok(&["tail", "-n", "12000", "long-tail"]);
    assert_eq!(larger.split(|&byte| byte == b'\n').count() - 1, 12_000);
    let compressed = s.sessions.join("long-tail.log");
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(&compressed)
        .unwrap();
    let end = file.metadata().unwrap().len();
    let record = logfile::next(&mut file, logfile::HEADER_SIZE, end)
        .unwrap()
        .unwrap();
    file.seek(SeekFrom::Start(record.end - 25)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    file.seek(SeekFrom::Start(record.end - 25)).unwrap();
    file.write_all(&byte).unwrap();
    assert_eq!(s.ok(&["tail", "-n", "3", "long-tail"]), expected.as_bytes());
    assert!(
        !s.run(&["tail", "-n", "40000", "long-tail"], b"")
            .status
            .success()
    );
    let legacy = s.sessions.join("plain.log");
    File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&legacy)
        .unwrap()
        .write_all(b"old plaintext\nrecent plaintext\n")
        .unwrap();
    assert_eq!(s.ok(&["tail", "-n", "1", "plain"]), b"recent plaintext\n");
    assert_eq!(
        fs::read(legacy).unwrap(),
        b"old plaintext\nrecent plaintext\n"
    );
}
