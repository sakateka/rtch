use crate::{Result, storage};
use clap::{Arg, ArgAction, Command, ValueHint};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;

// These are independent command-line switches, not session state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct Options {
    pub command: String,
    pub session: Option<PathBuf>,
    pub program: Vec<OsString>,
    pub quiet: bool,
    pub force: bool,
    pub all: bool,
    pub follow: bool,
    pub lines: usize,
    pub cap: usize,
    pub detach: Option<u8>,
    pub suspend: bool,
    pub redraw: String,
    pub clear: String,
    pub ansi: bool,
    pub wait: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            command: "help".into(),
            session: None,
            program: vec![],
            quiet: false,
            force: false,
            all: false,
            follow: false,
            lines: 10,
            cap: 1024 * 1024,
            detach: Some(28),
            suspend: true,
            redraw: "winch".into(),
            clear: "none".into(),
            ansi: true,
            wait: false,
        }
    }
}
pub(crate) fn size(s: &str) -> std::result::Result<usize, String> {
    let (digits, mult) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        _ => (s, 1),
    };
    let n = digits
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_mul(mult));
    n.filter(|&n| n <= 256 * 1024 * 1024)
        .ok_or_else(|| "invalid log size (maximum 256m)".into())
}
#[derive(Clone, Copy)]
enum Sessions {
    All,
    Live,
    Ended,
}
fn session(filter: Sessions) -> Arg {
    Arg::new("session")
        .value_name("SESSION")
        .value_parser(clap::value_parser!(PathBuf))
        .add(ArgValueCompleter::new(move |current: &OsStr| {
            let prefix = current.to_string_lossy();
            storage::entries()
                .unwrap_or_default()
                .into_iter()
                .filter(|(name, state)| {
                    name.starts_with(prefix.as_ref())
                        && match filter {
                            Sessions::All => true,
                            Sessions::Live => {
                                matches!(state, storage::State::Running | storage::State::Attached)
                            }
                            Sessions::Ended => {
                                matches!(state, storage::State::Ended | storage::State::Stale)
                            }
                        }
                })
                .map(|(name, _)| CompletionCandidate::new(name))
                .collect::<Vec<_>>()
        }))
}
fn program() -> Arg {
    Arg::new("program")
        .value_name("PROGRAM")
        .num_args(0..)
        .trailing_var_arg(true)
        .allow_hyphen_values(true)
        .value_parser(clap::value_parser!(OsString))
        .value_hint(ValueHint::CommandWithArguments)
}
fn switch(id: &'static str, short: char, help: &'static str) -> Arg {
    Arg::new(id)
        .short(short)
        .action(ArgAction::SetTrue)
        .help(help)
}
fn setting(id: &'static str, short: char, help: &'static str) -> Arg {
    Arg::new(id)
        .short(short)
        .long(id)
        .action(ArgAction::Set)
        .num_args(0..=1)
        .require_equals(true)
        .default_missing_value("true")
        .default_value("false")
        .value_parser(clap::value_parser!(bool))
        .help(help)
}
// Keep the declarative schema shared by parsing, help and completion.
#[allow(clippy::too_many_lines)]
pub fn command() -> Command {
    let mut cmd=Command::new("rtch").version(env!("CARGO_PKG_VERSION"))
        .about("Persistent terminal sessions; ended sessions require an explicit new")
        .after_help("Config: ~/.config/rtch/config — session_dir = /absolute/path\nWithout PROGRAM, starts $SHELL as a login shell. Ctrl+\\ detaches; or run rtch detach SESSION elsewhere.")
        .subcommand_negates_reqs(true)
        .arg(session(Sessions::All)).arg(program())
        .arg(setting("quiet",'q',"Suppress status messages").global(true))
        .arg(setting("no-detach",'E',"Disable the detach key").global(true))
        .arg(setting("no-suspend",'z',"Pass Ctrl+Z through").global(true))
        .arg(setting("no-ansi",'t',"Disable ANSI terminal cleanup").global(true))
        .arg(Arg::new("cap").short('C').value_name("SIZE").default_value("1m").value_parser(size).global(true).help("Log limit: bytes, k, m; 0 disables"))
        .arg(Arg::new("detach-key").short('e').value_name("KEY").default_value("^\\").global(true).value_parser(detach_key).help("Detach key, for example ^]"))
        .arg(Arg::new("redraw").short('r').default_value("winch").value_parser(["none","winch","ctrl_l"]).global(true))
        .arg(Arg::new("clear-mode").short('R').default_value("none").value_parser(["none","move"]).global(true));
    for (name, alias, help) in [
        (
            "new",
            "n",
            "Explicitly create a session, including an ended name",
        ),
        ("start", "s", "Create a detached session"),
        ("run", "", "Run the supervisor in the foreground"),
        (
            "open",
            "",
            "Attach or create; never restart ended sessions implicitly",
        ),
        ("__serve", "", "Internal supervisor"),
    ] {
        let mut sub = Command::new(name)
            .about(help)
            .arg(session(Sessions::All).required(true))
            .arg(program());
        if !alias.is_empty() {
            sub = sub.visible_alias(alias);
        }
        if name == "open" {
            sub = sub.hide(true);
        }
        if name == "__serve" {
            sub = sub.hide(true).arg(
                Arg::new("wait")
                    .long("wait")
                    .action(ArgAction::SetTrue)
                    .hide(true),
            );
        }
        cmd = cmd.subcommand(sub);
    }
    for (name, alias, help) in [
        ("attach", "a", "Attach to a live session"),
        (
            "detach",
            "",
            "Detach all clients without stopping the program",
        ),
        ("push", "p", "Pipe stdin into a session"),
        ("kill", "k", "Stop the session; escalate after 5 seconds"),
    ] {
        let mut sub = Command::new(name)
            .about(help)
            .arg(session(Sessions::Live).required(true));
        if !alias.is_empty() {
            sub = sub.visible_alias(alias);
        }
        if name == "kill" {
            sub = sub.arg(switch("force", 'f', "Send SIGKILL immediately").long("force"));
        }
        cmd = cmd.subcommand(sub);
    }
    cmd.subcommand(
        Command::new("list")
            .visible_aliases(["l", "ls"])
            .about("List running, attached, ended and stale sessions")
            .arg(switch(
                "all",
                'a',
                "Include ended sessions (already the default)",
            )),
    )
    .subcommand(Command::new("ended").about("List ended sessions only"))
    .subcommand(Command::new("current").about("Print the current session ancestry"))
    .subcommand(
        Command::new("clear")
            .about("Clear history; defaults to the current session")
            .arg(session(Sessions::All)),
    )
    .subcommand(
        Command::new("rm")
            .about("Remove ended/stale sessions and logs")
            .arg(
                session(Sessions::Ended)
                    .required_unless_present("all")
                    .conflicts_with("all"),
            )
            .arg(switch("all", 'a', "Remove all ended/stale sessions")),
    )
    .subcommand(
        Command::new("tail")
            .about("Display filtered history")
            .arg(session(Sessions::All).required(true))
            .arg(setting("follow", 'f', "Follow the log"))
            .arg(
                Arg::new("lines")
                    .short('n')
                    .default_value("10")
                    .value_parser(clap::value_parser!(usize)),
            ),
    )
}
pub(crate) fn detach_key(s: &str) -> std::result::Result<u8, String> {
    match s.as_bytes() {
        [b'^', b'?'] => Ok(127),
        [b'^', b] => Ok(b & 31),
        [b] => Ok(*b),
        _ => Err("expected one byte or ^X".into()),
    }
}
pub fn parse(args: Vec<OsString>) -> Result<Options> {
    parse_with(args, || Ok(crate::config::load()?.options))
}
fn parse_with(args: Vec<OsString>, defaults: impl FnOnce() -> Result<Options>) -> Result<Options> {
    let root =
        match command().try_get_matches_from(std::iter::once(OsString::from("rtch")).chain(args)) {
            Ok(m) => m,
            Err(e)
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                e.print()?;
                return Ok(Options {
                    command: "noop".into(),
                    ..Options::default()
                });
            }
            Err(e) => return Err(e.into()),
        };
    let (name, m) = root.subcommand().unwrap_or(("open", &root));
    let session = m.try_get_one::<PathBuf>("session").ok().flatten().cloned();
    if name == "open" && session.is_none() {
        command().print_help()?;
        return Ok(Options {
            command: "noop".into(),
            ..Options::default()
        });
    }
    let flag = |key: &str| {
        m.try_get_one::<bool>(key)
            .ok()
            .flatten()
            .copied()
            .unwrap_or(false)
    };
    let mut o = Options {
        command: name.into(),
        session,
        program: m
            .try_get_many::<OsString>("program")
            .ok()
            .flatten()
            .map(|v| v.cloned().collect())
            .unwrap_or_default(),
        quiet: flag("quiet"),
        force: flag("force"),
        all: flag("all"),
        follow: flag("follow"),
        lines: m
            .try_get_one::<usize>("lines")
            .ok()
            .flatten()
            .copied()
            .unwrap_or(10),
        cap: *m.get_one::<usize>("cap").expect("clap default"),
        detach: if flag("no-detach") {
            None
        } else {
            m.get_one::<u8>("detach-key").copied()
        },
        suspend: !flag("no-suspend"),
        ansi: !flag("no-ansi"),
        wait: flag("wait"),
        redraw: m.get_one::<String>("redraw").expect("clap default").clone(),
        clear: m
            .get_one::<String>("clear-mode")
            .expect("clap default")
            .clone(),
    };
    let defaults = defaults()?;
    let explicit = |key| m.value_source(key) == Some(clap::parser::ValueSource::CommandLine);
    macro_rules! inherit {
        ($($field:ident => $key:literal),* $(,)?) => { $(
            if !explicit($key) { o.$field = defaults.$field; }
        )* };
    }
    inherit!(quiet => "quiet", cap => "cap", suspend => "no-suspend",
        ansi => "no-ansi", redraw => "redraw", clear => "clear-mode");
    if name == "tail" {
        inherit!(lines => "lines", follow => "follow");
    }
    if !explicit("detach-key") {
        o.detach = if explicit("no-detach") {
            if flag("no-detach") {
                None
            } else {
                defaults.detach.or(Some(28))
            }
        } else {
            defaults.detach
        };
    }
    Ok(o)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: Vec<OsString>) -> Result<Options> {
        parse_with(args, || Ok(Options::default()))
    }
    #[test]
    fn schema() {
        command().debug_assert();
    }
    #[test]
    fn config_defaults_and_cli_overrides() {
        let text = r"quiet = true
log_size = 2k
detach_key = none
suspend = false
ansi = false
redraw = none
clear_mode = move
tail_lines = 3
tail_follow = true";
        let parse = |args: &[&str]| {
            parse_with(args.iter().map(OsString::from).collect(), || {
                Ok(crate::config::Config::parse(text)?.options)
            })
            .unwrap()
        };
        let o = parse(&["tail", "work"]);
        assert!(o.quiet && o.follow && !o.ansi && !o.suspend);
        assert_eq!((o.cap, o.lines, o.detach), (2048, 3, None));
        assert_eq!((&*o.redraw, &*o.clear), ("none", "move"));
        let o = parse(&[
            "--quiet=false",
            "-C",
            "0",
            "-e",
            "^]",
            "-r",
            "winch",
            "-R",
            "none",
            "--no-suspend=false",
            "--no-ansi=false",
            "tail",
            "--follow=false",
            "-n",
            "0",
            "work",
        ]);
        assert!(!o.quiet && !o.follow && o.ansi && o.suspend);
        assert_eq!((o.cap, o.lines, o.detach), (0, 0, Some(29)));
        assert_eq!((&*o.redraw, &*o.clear), ("winch", "none"));
        assert_eq!(
            parse(&["--no-detach=false", "attach", "work"]).detach,
            Some(28)
        );
        assert_eq!(parse(&["-E", "attach", "work"]).detach, None);
        let o = parse_with(
            ["--no-detach=false", "attach", "work"]
                .map(Into::into)
                .to_vec(),
            || Ok(crate::config::Config::parse("detach_key = ^]")?.options),
        )
        .unwrap();
        assert_eq!(o.detach, Some(29));
        assert!(
            super::parse_with(vec!["--help".into()], || panic!(
                "help must not load config"
            ))
            .is_ok()
        );
    }
    #[test]
    fn argument_parsing() {
        for mode in ["-a", "-A", "-c", "-n", "-N", "-p", "-k", "-l", "-i"] {
            assert!(parse(vec![mode.into(), "work".into()]).is_err());
        }
        for prefix in [vec!["new"], vec!["-q", "new"]] {
            let args = prefix
                .into_iter()
                .chain(["-C", "4m", "work", "bash", "-l"])
                .map(Into::into)
                .collect();
            let o = parse(args).unwrap();
            assert_eq!(o.cap, 4 * 1024 * 1024);
            assert_eq!(o.program, vec!["bash", "-l"]);
        }
        let o = parse(["work", "bash", "-C", "file"].map(Into::into).to_vec()).unwrap();
        assert_eq!(o.program, vec!["bash", "-C", "file"]);
        assert!(parse(["rm", "-a", "work"].map(Into::into).to_vec()).is_err());
        assert!(size("-1").is_err());
        assert!(size("999999999999999999999m").is_err());
    }
}
