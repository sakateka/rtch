# rtch

[Русский](README.md)

A terminal session manager written in Rust for Linux. A supervisor owns the
child process's PTY; clients connect over a Unix socket. Client or SSH
disconnects leave the process running. Live output passes through unfiltered;
terminal emulation and pane management are not implemented.

## Build and install

Requires stable Rust 1.88 or newer.

```sh
cargo build --release --locked
install -Dm755 target/release/rtch ~/.local/bin/rtch
```

`~/.local/bin` must be in `PATH`.

## CLI

```text
rtch [OPTIONS] SESSION [PROGRAM [ARGS...]]
rtch [OPTIONS] COMMAND [COMMAND_OPTIONS] [SESSION] [PROGRAM [ARGS...]]
```

Without `PROGRAM`, rtch starts `$SHELL` (or `/bin/sh`) as a login shell.
Bash loads `~/.profile` unless `~/.bash_profile` or `~/.bash_login` takes precedence.
Explicit programs retain their startup behavior; reattaching does not reload profiles.
rtch options precede `PROGRAM`; subsequent arguments are passed to the program.

```sh
rtch new monitor htop
# Detach: Ctrl+\
rtch attach monitor

rtch new dev bash -l
rtch start build make
rtch tail -f build
```

| Command | Behavior |
| --- | --- |
| `rtch SESSION` | Attach to a live session or create a missing one |
| `rtch new SESSION [PROGRAM...]` | Create and attach; explicitly restart ended/stale sessions |
| `rtch start SESSION [PROGRAM...]` | Create without attaching |
| `rtch run SESSION [PROGRAM...]` | Run the supervisor in the foreground; return the program's exit code |
| `rtch attach SESSION` | Attach to a live session |
| `rtch detach SESSION` | Disconnect all clients without terminating the program |
| `rtch list` / `ended` | List all sessions / ended sessions only |
| `rtch tail [-f] [-n LINES] SESSION` | Read history; `-f` follows new output |
| `rtch push SESSION` | Forward stdin to session input |
| `rtch clear [SESSION]` | Clear the log and history buffer; defaults to the current session |
| `rtch kill [-f] SESSION` | SIGTERM, then SIGKILL after 5 s; `-f` sends SIGKILL immediately |
| `rtch rm SESSION` / `rm -a` | Remove one / all ended/stale sessions and their history |
| `rtch current` | Print the current session name or nested session chain |

`Ctrl+\` detaches the current client. If intercepted by the application,
`rtch detach monitor` can be run from another terminal. Screen contents may
remain after detach. Exiting the child process ends the session.

## Session states

| State | Meaning |
| --- | --- |
| `running` | Supervisor running, no clients attached |
| `attached` | Clients attached |
| `ended` | Session ended; state and/or log retained |
| `stale` | Socket exists without a serving process |

Restart ended sessions explicitly with `rtch new work`; `rtch work` and
`attach` never restart them automatically. Read history with `tail`.
`rtch rm work` removes an ended session; `rtch rm -a` removes all ended/stale
sessions, keeping live ones. State and exit status persist in `.ended` even
without logging; a leftover socket without a process is `[stale]`.

Reboots preserve files, not processes.
Full reference: `rtch --help`, `rtch COMMAND --help`.

## Configuration and storage

Storage defaults to `~/.cache/rtch/`. Example directory setup:

```sh
mkdir -p ~/.config/rtch /mnt/data/rtch
chmod 700 /mnt/data/rtch
```

File: `~/.config/rtch/config`.

```ini
session_dir = /mnt/data/rtch
quiet = false
log_size = 1m
detach_key = ^\
suspend = true
ansi = true
redraw = winch
clear_mode = none
tail_lines = 10
tail_follow = false
```

Precedence: CLI → config → defaults. All values above except the example
`session_dir` are defaults.

| Key | CLI | Values |
| --- | --- | --- |
| `session_dir` | Absolute `SESSION` | Absolute, unquoted path |
| `quiet` | `-q` | `true/false`, suppress status messages |
| `log_size` | `-C SIZE` | Bytes, `k/m` suffixes; `0` disables logging; maximum `256m` |
| `detach_key` | `-e KEY`, `-E` | One byte, `^X`; `none` disables the key |
| `suspend` | `-z` disables | `true/false`, local Ctrl+Z handling |
| `ansi` | `-t` disables | `true/false`, ANSI terminal reset on detach |
| `redraw` | `-r MODE` | `none/winch/ctrl_l` |
| `clear_mode` | `-R MODE` | `none/move` |
| `tail_lines` | `tail -n LINES` | Nonnegative integer |
| `tail_follow` | `tail -f` | `true/false` |

Boolean options accept explicit overrides: `--quiet=false`, `--no-detach=false`,
`--no-suspend=false`, `--no-ansi=false`, or `tail --follow=false`.
Invalid values and unknown keys are rejected. `force`, `all`, session names,
and launched commands are not config settings.

Use an absolute, unquoted path. Spaces are allowed; `~` and variables are
not expanded. Blank lines and `#` comments are ignored. An absolute
`XDG_CONFIG_HOME` moves configuration to `$XDG_CONFIG_HOME/rtch/config`.

The directory must belong to the current UID and prohibit group/other writes.
An absolute session path overrides the directory, not other settings; `list` and `rm -a`
use the configured directory. Colons, control characters, and `.log`/`.ended`
suffixes are reserved in names.

## Bash completion

Current shell:

```bash
source <(COMPLETE=bash rtch)
```

Automatic loading with `bash-completion` enabled:

```bash
completion_dir="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions"
mkdir -p "$completion_dir"
COMPLETE=bash rtch > "$completion_dir/rtch"
```

Tab completes commands, options, `-r`/`-R` values, programs, and sessions,
including names with spaces. Suggestions: live sessions for
`attach/detach/push/kill`, ended/stale for `rm`, all for `tail/clear` and
creation. Sessions refresh on every Tab.

## History and limits

Logs default to 1 MiB; exceeding twice the limit retains the latest 1 MiB.
`-C 4m` changes the limit; `-C 0` keeps only 128 KiB of memory history and
a state file. Replay filters terminal queries and unsupported control
sequences; live output is unchanged.

Detach and `SIGHUP` restore terminal settings; `SIGKILL` cannot be handled.
Up to 64 connections; clients with overflowing queues are disconnected.
Logs may contain secrets: private files and UID checks do not protect
against processes running as the same user.

## Tests

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
RTCH_IO_BACKEND=poll cargo test --locked
RTCH_IO_BACKEND=uring cargo test --locked
```

`cargo test` runs both unit and integration tests. Requires Bash, standard
Linux utilities, Unix sockets, and `/dev/ptmx`. Tests use temporary directories
and terminate only their own sessions.

I/O backend: `RTCH_IO_BACKEND=auto` (default) uses io_uring readiness on
Linux 5.11+ and falls back to `poll` if unavailable. `uring` requires io_uring;
`poll` forces compatibility mode. Reads and writes remain synchronous.
