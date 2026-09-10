//! Real process, Unix socket and PTY checks; run with cargo test.
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, symlink},
            net::UnixStream,
        },
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

// Reuse the production Unix boundary instead of duplicating unsafe PTY wrappers.
#[allow(dead_code)]
#[path = "../src/os.rs"]
mod os;

const BINARY: &str = env!("CARGO_BIN_EXE_rtch");
static NEXT: AtomicUsize = AtomicUsize::new(0);

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
struct Sessions {
    root: PathBuf,
    sessions: PathBuf,
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
        let suite = Self { root, sessions };
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
        fs::read(self.sessions.join(format!("{name}.log"))).unwrap_or_default()
    }
    fn attach(&self, args: &[&str]) -> Client {
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
        let child = self
            .command(BINARY)
            .arg("-q")
            .args(args)
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
        let t = os::term(self.slave.as_raw_fd()).unwrap();
        let o = &self.original;
        assert_eq!(
            (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_cc),
            (o.c_iflag, o.c_oflag, o.c_cflag, o.c_lflag, o.c_cc)
        );
    }
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
    assert!(s.log("rotate").len() <= 2048);
    s.ok(&["clear", "rotate"]);
    assert!(s.log("rotate").is_empty());
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
