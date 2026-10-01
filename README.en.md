# rtch

[Русский](README.md)

A terminal session manager written in Rust for Linux. A supervisor owns the
child process's PTY; clients connect over a Unix socket. Client or SSH
disconnects leave the process running. Live output passes through unfiltered;
the picker reconstructs terminal history in memory.

## Build and install

Requires stable Rust 1.88 or newer.

```sh
cargo build --release --locked
install -Dm755 target/release/rtch ~/.local/bin/rtch
```

`~/.local/bin` must be in `PATH`.

## CLI

```text
rtch [OPTIONS]
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
| `rtch` / `rtch pick` | Browse sessions and their Beginning/Ending previews |
| `rtch shell-init` | Print a POSIX login-shell picker hook |
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

## Session picker and login hook

Bare `rtch` and `rtch pick` show all session states, most recent activity first
(name breaks ties). At normal terminal size the list shows eight rows and scrolls.
Beginning starts at the first retained line; Ending starts at the latest lines.
Selecting another session resets both previews' vertical and horizontal offsets. A bounded vt100 screen applies
cursor movement, erasure, backspace and redraws;
OSC/DCS payloads and unsafe controls are omitted. Ratatui frames the panes, and
terminal rows are joined into logical lines only when vt100 marks a soft wrap.
Unmarked breaks before wide glyphs can remain. Long lines are clipped
inside their panes; horizontal scrolling reveals retained text beyond the edge.
Clipping preserves graphemes and blanks partially visible wide characters.
Missing, empty, or unsafe logs
show a message. The virtual screen uses the invoking terminal’s dimensions and keeps up to
1024 scrollback rows. Historical terminal sizes are not recorded, so resized
or rotated full-screen history can reconstruct approximately. Non-TTY invocations
print command guidance without entering raw mode.

- `Tab` / `Shift+Tab`: cycle List → Beginning → Ending; the focused preview expands.
- Up/Down, `j`/`о` (down), `k`/`л` (up), `PageUp`/`PageDown`, `Home`/`End`: move or scroll logical rows in the focused pane.
- Left/Right, `h`/`р` (left), or `l`/`д` (right): scroll the focused preview horizontally by columns.
  Beginning and Ending keep independent offsets; these keys do nothing in the list.
- `Enter`: attach a detached running session, or restart an ended/stale session
  under the same name while preserving retained history within the configured log
  limits. Attached sessions stay busy in the picker.
  Restart starts the current `$SHELL` as a fresh login shell in the invoking
  directory; it does not resume the original program or process.
- `Insert` (also `n`/`т`): edit a unique suggested name, then `Enter` creates and attaches. Arrows,
  `Home`/`End`, Backspace, and Delete edit graphemes; `Esc` cancels naming.
  `Insert` and `F2` do nothing while editing the name.
  Latin and Russian shortcut letters are literal name characters, except the configured
  printable detach key: its original character still exits. Names are limited to 107 bytes;
  pasting a newline does not submit the name.
- `Delete` (also `d`/`в`): delete the selected ended/stale session and its artifacts, then select a neighbour.
  Live sessions, including attached ones, cannot be deleted.
- `F2` (also `c`/`с`): clear retained logs and live replay history, preserving the session,
  running process, and any attached clients. Future output continues logging
  under the configured cap. Delete and clean refresh the previews and keep the picker open.
- `q`/`й`, `Esc`, `Ctrl+C`, `Ctrl+D`, or the configured detach key: return to the invoking shell.

Arrows, `Tab`/`Shift+Tab`, `Enter`, `Escape`, `Insert`, `Delete`, and `F2` provide
complete picker control in any keyboard layout. The lowercase English QWERTY and
standard Russian JCUKEN shortcuts above remain optional additional controls;
name characters are kept as typed.
Bracketed paste never triggers shortcut letters or function-key actions. `Ctrl+C`, `Ctrl+D`, and a
configured control detach key still exit during paste. After opening a session, shortcut
letters and all navigation, `Insert`, `Delete`, and `F2` bytes pass unchanged to the application,
except for the configured detach byte when detachment is enabled.

Small terminals show a resize hint; delete and clean require the full picker layout.
If the session directory is busy, retry the action after it becomes available.
Creation waits for a resize if the proposed
name cannot be displayed. Picker exits,
errors, and handled termination signals restore terminal settings and the previous screen.

Inside an rtch session (`RTCH_SESSION` is nonempty), opening a picker, creating a
session, or attaching to another session is rejected before terminal or session
changes. Management commands such as `list`, `current`, `tail`, `clear`, `push`,
`detach`, and `kill` remain available.

To open the picker when an interactive login shell starts, back up your login
startup file, then append the output of `rtch shell-init` after its PATH setup:

```sh
rtch shell-init >> ~/.profile
```

Use the startup file your shell actually reads (for Bash, a `.bash_profile` or
`.bash_login` takes precedence over `.profile`). The POSIX hook calls a child
process, so leaving the picker or detaching returns to that shell. It skips
noninteractive/non-TTY shells, rtch children (`RTCH_SESSION`), `RTCH_BYPASS=1`,
`TERM=dumb`, and an unavailable executable. Startup continues if the picker
fails, including profiles using `set -e`. Bypass one login with
`RTCH_BYPASS=1 bash -l`.

## Session states

| State | Meaning |
| --- | --- |
| `running` | Supervisor running, no clients attached |
| `attached` | Clients attached |
| `ended` | Session ended; state and/or log retained |
| `stale` | Socket exists without a serving process |

Restart ended sessions with picker `Enter` or explicitly with `rtch new work`;
named `rtch work` and `attach` never restart them automatically. Picker restart
starts the current login shell in the invoking directory. Read history with `tail`.
`rtch rm work` removes an ended session; `rtch rm -a` removes all ended/stale
sessions, keeping live ones. State and exit status persist in `.ended` even
without logging; a leftover socket without a process is `[stale]`.

Updated supervisors atomically refuse a picker attachment when another client is
attached. For legacy supervisors, the picker uses a short capability timeout and
rechecks the socket state before the existing attach operation; that fallback
cannot guarantee exclusive admission if another client attaches simultaneously.

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
use the configured directory. Colons, control characters, and `.log`/`.ended`/`.head`
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
a state file, without creating a persistent prefix. A private `.head` file retains
the first `min(log limit, 64 KiB)` output bytes across log rotation and explicit
restarts. Beginning emulates at most 64 KiB of this prefix; Ending emulates at most
128 KiB of recent output including bounded parsing context, omitting an initial
partial line. Beginning stops emulation before its earliest rows can be evicted
from bounded scrollback. Reconstructed screen and scrollback rows support
independent paging. `clear` empties both histories; `rm` removes both.
Legacy sessions without `.head` show their earliest retained log output; a lost
beginning cannot be recovered. A control string whose opening was lost in
rotation or lies outside the bounded window can leave ambiguous text fragments;
recognized complete strings and orphaned terminators are handled conservatively.
Sessions with disabled logs have no disk preview
unless history from an earlier run was retained. Replay filters terminal queries
and unsupported control
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
