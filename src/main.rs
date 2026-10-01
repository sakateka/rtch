mod cli;
mod client;
mod config;
mod history;
mod logfile;
mod os;
mod picker;
mod reactor;
mod server;
mod storage;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(message.into().into())
}
fn current() -> Option<String> {
    std::env::var("RTCH_SESSION").ok().filter(|s| !s.is_empty())
}
#[allow(clippy::naive_bytecount)] // Avoid a dependency for counting bounded newlines.
fn tail(o: &cli::Options, path: &Path) -> Result<()> {
    let mut file = storage::open_file(&storage::side(path, ".log"), false)?;
    // Start with a small decoded suffix; expand only for requested filtered lines.
    let mut limit = storage::PREVIEW_LIMIT;
    let (window, mut filter, rendered) = loop {
        let window = logfile::read(&mut file, limit, false)?;
        let mut filter = history::Filter::default();
        let start = history::emulator_boundary(&window.bytes, window.cut);
        let rendered = filter.feed(&window.bytes[start..]);
        let lines = rendered.iter().filter(|&&c| c == b'\n').count();
        if o.lines == 0 || lines > o.lines || !window.earlier || limit == 256 * 1024 * 1024 {
            break (window, filter, rendered);
        }
        limit = (limit * 2).min(256 * 1024 * 1024);
    };
    let mut count = 0;
    let mut start = 0;
    for (i, &c) in rendered.iter().enumerate().rev() {
        if c == b'\n' && i + 1 < rendered.len() {
            count += 1;
            if count == o.lines {
                start = i + 1;
                break;
            }
        }
    }
    if o.lines > 0 {
        io::stdout().write_all(&rendered[start..])?;
    }
    if o.follow {
        os::signals(false)?;
        let mut follower = logfile::Follower::new(file, storage::side(path, ".log"), &window)?;
        while os::take_signals() & 1 == 0 {
            let progressed = follower.poll(|bytes, reset| {
                if reset {
                    filter = history::Filter::default();
                }
                io::stdout().write_all(&filter.feed(bytes))
            })?;
            if !progressed {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Ok(())
}
fn picker_choice(o: &cli::Options) -> Result<()> {
    picker::pick(o, |choice, established| {
        let mut selected = o.clone();
        selected.detached_only = true;
        match choice {
            picker::Choice::Attach(path) | picker::Choice::Restart(path) => {
                let path = storage::resolve(&path)?;
                selected.exclusive = false;
                match storage::state(&path)? {
                    storage::State::Running => {}
                    storage::State::Attached => {
                        return fail("Session is attached; busy with another client.");
                    }
                    storage::State::Ended | storage::State::Stale => {
                        server::start(&selected, &path, true)?;
                    }
                    storage::State::Missing => return fail("Session is missing."),
                }
                client::attach_with_status(&selected, &path, established)
            }
            picker::Choice::Create(path) => {
                let path = storage::resolve(&path)?;
                selected.exclusive = true;
                server::start(&selected, &path, true)?;
                client::attach_with_status(&selected, &path, established)
            }
            picker::Choice::Exit => unreachable!("handled above"),
        }
    })
}
fn clear_session(path: &Path) -> Result<()> {
    if matches!(
        storage::state(path)?,
        storage::State::Running | storage::State::Attached
    ) {
        client::control(path, server::CLEAR, false)
    } else {
        storage::clear_files(path)
    }
}
fn run() -> Result<()> {
    let o = cli::parse(std::env::args_os().skip(1).collect())?;
    if matches!(
        o.command.as_str(),
        "pick" | "new" | "start" | "run" | "attach" | "open" | "__serve"
    ) && std::env::var_os("RTCH_SESSION").is_some_and(|session| !session.is_empty())
    {
        return fail("already inside rtch; nested session startup or attachment is not allowed");
    }
    match o.command.as_str() {
        "noop" => return Ok(()),
        "shell-init" => return picker::shell_init(),
        "pick" => return picker_choice(&o),
        "version" => {
            println!("rtch {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        "current" => {
            let chain = current().ok_or("not inside an rtch session")?;
            println!(
                "{}",
                chain
                    .split(':')
                    .map(|s| Path::new(s)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" > ")
            );
            return Ok(());
        }
        "list" | "ended" => return storage::list(o.command == "ended"),
        "rm" if o.all => return storage::remove_all(),
        _ => {}
    }
    let session = o
        .session
        .clone()
        .or_else(|| {
            if o.command == "clear" {
                current().and_then(|s| s.rsplit(':').next().map(PathBuf::from))
            } else {
                None
            }
        })
        .ok_or("no session specified")?;
    let path = storage::resolve(&session)?;
    match o.command.as_str() {
        "__serve" => server::serve(&o, &path, true, o.wait).map(|_| ()),
        "run" => {
            let code = server::serve(&o, &path, false, false)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        "attach" => client::attach(&o, &path),
        "open" => match storage::state(&path)? {
            storage::State::Running | storage::State::Attached => client::attach(&o, &path),
            storage::State::Missing => {
                os::term(0).map_err(|_| "attaching requires a terminal")?;
                server::start(&o, &path, true)?;
                client::attach(&o, &path)
            }
            _ => fail(
                "session has ended or is stale; use tail for history, or new to explicitly restart",
            ),
        },
        "new" => {
            os::term(0).map_err(|_| "attaching requires a terminal")?;
            server::start(&o, &path, true)?;
            client::attach(&o, &path)
        }
        "start" => {
            server::start(&o, &path, false)?;
            if !o.quiet {
                println!("rtch: session '{}' started", session.display());
            }
            Ok(())
        }
        "push" => client::push(&path),
        "detach" => client::control(&path, server::DETACH, false),
        "kill" => client::control(&path, server::KILL, o.force),
        "tail" => tail(&o, &path),
        "clear" => clear_session(&path),
        "rm" => storage::remove(&path),
        _ => fail("unknown command"),
    }
}
fn main() {
    clap_complete::CompleteEnv::with_factory(cli::command).complete();
    if let Err(e) = run() {
        eprintln!("rtch: {e}");
        std::process::exit(1);
    }
}
