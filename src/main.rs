mod cli;
mod client;
mod config;
mod history;
mod os;
mod reactor;
mod server;
mod storage;
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(message.into().into())
}
fn current() -> Option<String> {
    std::env::var("RTCH_SESSION").ok().filter(|s| !s.is_empty())
}
fn tail(o: &cli::Options, path: &Path) -> Result<()> {
    let mut file = storage::open_file(&storage::side(path, ".log"), false)?;
    // Bound memory even when reading a log produced by another version.
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(256 * 1024 * 1024)))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(256 * 1024 * 1024)
        .read_to_end(&mut bytes)?;
    let mut filter = history::Filter::default();
    let rendered = filter.feed(&bytes);
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
        let mut buf = [0; 8192];
        while os::take_signals() & 1 == 0 {
            if file.metadata()?.len() < file.stream_position()? {
                file.rewind()?;
                filter = history::Filter::default();
            }
            let n = file.read(&mut buf)?;
            if n > 0 {
                io::stdout().write_all(&filter.feed(&buf[..n]))?;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Ok(())
}
fn run() -> Result<()> {
    let o = cli::parse(std::env::args_os().skip(1).collect())?;
    match o.command.as_str() {
        "noop" => return Ok(()),
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
        "clear" => {
            if matches!(
                storage::state(&path)?,
                storage::State::Running | storage::State::Attached
            ) {
                client::control(&path, server::CLEAR, false)
            } else {
                match storage::open_file(&storage::side(&path, ".log"), false) {
                    Ok(f) => {
                        drop(f);
                        storage::open_file(&storage::side(&path, ".log"), true)?.set_len(0)?;
                        Ok(())
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
        }
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
