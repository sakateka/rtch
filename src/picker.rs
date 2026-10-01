//! A bounded text browser; live sessions still use the ordinary attach client.
use crate::{Result, cli::Options, history, os, storage};
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Widget},
};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub enum Choice {
    Attach(PathBuf),
    Restart(PathBuf),
    Create(PathBuf),
    Exit,
}

pub const SHELL_INIT: &str = r#"# >>> rtch login-shell picker >>>
# Append after PATH setup.
case $- in
    *i*)
        if [ -t 0 ] && [ -t 1 ] && [ -z "${RTCH_SESSION-}" ] &&
            [ "${RTCH_BYPASS-}" != 1 ] && [ "${TERM-}" != dumb ] &&
            command -v rtch >/dev/null 2>&1; then
            rtch pick || :
        fi
        ;;
esac
# <<< rtch login-shell picker <<<
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    List,
    Beginning,
    Ending,
}
impl Focus {
    fn cycle(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::List, false) | (Self::Ending, true) => Self::Beginning,
            (Self::Beginning, false) | (Self::List, true) => Self::Ending,
            _ => Self::List,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Escape,
    Enter,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
    Insert,
    Delete,
    F2,
    Char(char),
    Paste(char),
}
#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
    since: Option<Instant>,
    paste: bool,
    discard: bool,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut keys = Vec::new();
        for &b in bytes {
            if self.paste {
                self.pending.push(b);
                self.decode_paste(&mut keys);
                continue;
            }
            if b == 27 && !self.pending.is_empty() {
                if self.pending == [27] {
                    keys.push(Key::Escape);
                }
                self.pending.clear();
            }
            if self.discard && b != 27 {
                if (0x40..=0x7e).contains(&b) {
                    self.discard = false;
                }
                continue;
            }
            if b == 27 {
                self.discard = false;
            }
            self.since = Some(Instant::now());
            self.pending.push(b);
            self.decode(&mut keys);
        }
        keys
    }
    fn decode_paste(&mut self, keys: &mut Vec<Key>) {
        const END: &[u8] = b"\x1b[201~";
        loop {
            if self.pending.is_empty() {
                return;
            }
            if END.starts_with(&self.pending) {
                if self.pending == END {
                    self.paste = false;
                    self.pending.clear();
                }
                return;
            }
            let take = match std::str::from_utf8(&self.pending) {
                Ok(text) => text.chars().next().expect("paste data").len_utf8(),
                Err(e) if e.valid_up_to() > 0 => {
                    std::str::from_utf8(&self.pending[..e.valid_up_to()])
                        .expect("valid prefix")
                        .chars()
                        .next()
                        .expect("paste data")
                        .len_utf8()
                }
                Err(e) if e.error_len().is_some() => {
                    self.pending.drain(..e.error_len().expect("invalid UTF-8"));
                    continue;
                }
                Err(_) => return,
            };
            for c in std::str::from_utf8(&self.pending[..take])
                .expect("valid prefix")
                .chars()
            {
                if matches!(c, '\r' | '\n' | '\t') {
                    keys.push(Key::Paste(' '));
                } else if !c.is_control() {
                    keys.push(Key::Paste(c));
                }
            }
            self.pending.drain(..take);
        }
    }
    fn decode(&mut self, keys: &mut Vec<Key>) {
        let first = self.pending[0];
        if first == 27 {
            if self.pending.len() == 1 {
                return;
            }
            if !matches!(self.pending[1], b'[' | b'O') {
                keys.push(Key::Escape);
                self.pending.remove(0);
                self.decode(keys);
                return;
            }
            if self.pending.len() < 3 {
                return;
            }
            let last = *self.pending.last().expect("nonempty decoder");
            if !(0x40..=0x7e).contains(&last) {
                if self.pending.len() >= 32 {
                    self.discard = true;
                    self.pending.clear();
                }
                return;
            }
            let key = match self.pending.as_slice() {
                b"\x1b[2~" => Some(Key::Insert),
                b"\x1b[3~" => Some(Key::Delete),
                b"\x1bOQ" | b"\x1b[12~" | b"\x1b[1Q" | b"\x1b[1;1Q" => Some(Key::F2),
                _ => match &self.pending[2..] {
                    b"200~" => {
                        self.paste = true;
                        None
                    }
                    b"201~" => {
                        self.paste = false;
                        None
                    }
                    b"A" => Some(Key::Up),
                    b"B" => Some(Key::Down),
                    b"C" => Some(Key::Right),
                    b"D" => Some(Key::Left),
                    b"H" | b"1~" | b"7~" => Some(Key::Home),
                    b"F" | b"4~" | b"8~" => Some(Key::End),
                    b"5~" => Some(Key::PageUp),
                    b"6~" => Some(Key::PageDown),
                    b"Z" => Some(Key::BackTab),
                    _ => None,
                },
            };
            keys.extend(key);
            self.pending.clear();
        } else {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    let c = text.chars().next().expect("nonempty decoder");
                    keys.push(match c {
                        '\r' | '\n' => Key::Enter,
                        '\t' => Key::Tab,
                        '\u{7f}' | '\u{8}' => Key::Backspace,
                        _ => Key::Char(c),
                    });
                    self.pending.clear();
                }
                Err(e) if e.error_len().is_some() => {
                    self.pending.clear();
                }
                Err(_) => {}
            }
        }
    }
    fn expire(&mut self) -> Option<Key> {
        if !self.paste
            && self.pending == [27]
            && self
                .since
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(80))
        {
            self.since = None;
            self.pending.clear();
            return Some(Key::Escape);
        }
        None
    }
}

struct Entry {
    name: String,
    state: storage::State,
    activity: SystemTime,
}
struct Naming {
    text: String,
    cursor: usize,
}
struct Model {
    dir: PathBuf,
    entries: Vec<Entry>,
    selected: usize,
    top: usize,
    focus: Focus,
    beginning: String,
    ending: String,
    raw: [Option<Vec<u8>>; 2],
    virtual_size: (u16, u16),
    offsets: [usize; 2],
    columns: [usize; 2],
    naming: Option<Naming>,
    message: String,
}
#[derive(Clone, Copy)]
struct Layout {
    height: usize,
    list: usize,
    beginning: usize,
    ending: usize,
}
impl Layout {
    fn new(height: usize, focus: Focus) -> Self {
        let list = if focus == Focus::List { 8 } else { 1 };
        let available = height.saturating_sub(list + 10);
        let (beginning, ending) = match focus {
            Focus::List => (available / 2, available - available / 2),
            Focus::Beginning => (available.saturating_sub(1), 1),
            Focus::Ending => (1, available.saturating_sub(1)),
        };
        Self {
            height,
            list,
            beginning,
            ending,
        }
    }
}
fn label(state: storage::State) -> &'static str {
    match state {
        storage::State::Running => "running",
        storage::State::Attached => "attached",
        storage::State::Ended => "ended",
        storage::State::Stale => "stale",
        storage::State::Missing => "missing",
    }
}
fn preview(path: &std::path::Path, beginning: bool, size: (u16, u16)) -> (String, Option<Vec<u8>>) {
    match storage::preview_context(path, beginning) {
        Ok(bytes) => (emulated(&bytes, size, beginning), Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (
            "(History unavailable: logging disabled or no retained log.)".into(),
            None,
        ),
        Err(e) => (format!("(History unavailable: {e})"), None),
    }
}
fn emulated(bytes: &[u8], size: (u16, u16), beginning: bool) -> String {
    let text = history::terminal_preview(bytes, size.0, size.1, beginning);
    if text.trim().is_empty() {
        "(No output recorded.)".into()
    } else {
        text
    }
}

impl Model {
    fn entries(dir: &std::path::Path) -> Result<Vec<Entry>> {
        let mut entries = storage::entries_in(dir)?
            .into_iter()
            .map(|(name, state)| {
                let path = dir.join(&name);
                let activity = [
                    path.clone(),
                    storage::side(&path, ".log"),
                    storage::side(&path, ".ended"),
                ]
                .into_iter()
                .filter_map(|p| fs::symlink_metadata(p).ok()?.modified().ok())
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
                Entry {
                    name,
                    state,
                    activity,
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            b.activity
                .cmp(&a.activity)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }
    fn load() -> Result<Self> {
        let dir = storage::directory()?;
        let entries = Self::entries(&dir)?;
        let window = os::size(0);
        let mut model = Self {
            dir,
            entries,
            selected: 0,
            top: 0,
            focus: Focus::List,
            beginning: String::new(),
            ending: String::new(),
            raw: [None, None],
            virtual_size: (window.ws_row.clamp(1, 200), window.ws_col.clamp(1, 512)),
            offsets: [0, 0],
            columns: [0, 0],
            naming: None,
            message: String::new(),
        };
        model.select();
        Ok(model)
    }
    fn select(&mut self) {
        if let Some(entry) = self.entries.get(self.selected) {
            let path = self.dir.join(&entry.name);
            (self.beginning, self.raw[0]) = preview(&path, true, self.virtual_size);
            (self.ending, self.raw[1]) = preview(&path, false, self.virtual_size);
        } else {
            self.beginning = "(No sessions. Press Insert to create one.)".into();
            self.ending.clone_from(&self.beginning);
            self.raw = [None, None];
        }
        self.offsets = [0, usize::MAX];
        self.columns = [0, 0];
        self.message.clear();
    }
    fn refresh(&mut self, reset: bool) -> Result<()> {
        let selected = self.entries.get(self.selected).map(|e| e.name.clone());
        let entries = Self::entries(&self.dir)?;
        let position = entries
            .iter()
            .position(|e| Some(&e.name) == selected.as_ref());
        self.selected = position.unwrap_or(self.selected.min(entries.len().saturating_sub(1)));
        self.entries = entries;
        let offsets = self.offsets;
        let columns = self.columns;
        self.select();
        if position.is_some() && !reset {
            self.offsets = offsets;
            self.columns = columns;
        }
        Ok(())
    }
    fn manage(&mut self, delete: bool) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        let path = self.dir.join(&entry.name);
        let action = if delete { "delete" } else { "clean" };
        let lock = match storage::try_lock_parent(&path) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                self.message = "Session directory is busy; retry delete/clean.".into();
                return;
            }
            Err(e) => {
                self.message = format!("Cannot {action}: {e}");
                return;
            }
        };
        let result = (|| {
            let state = storage::state(&path)?;
            if delete {
                if !matches!(state, storage::State::Ended | storage::State::Stale) {
                    return crate::fail(format!(
                        "Session is {}; only ended/stale sessions can be deleted.",
                        label(state)
                    ));
                }
                storage::remove(&path)
            } else if state == storage::State::Missing {
                crate::fail("Session is missing.")
            } else {
                crate::clear_session(&path)
            }
        })();
        drop(lock);
        if delete && result.is_ok() {
            self.entries.remove(self.selected);
            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
            self.select();
        }
        let mut message = match &result {
            Ok(()) if delete => "Session deleted.".into(),
            Ok(()) => "Logs cleaned; future output continues logging.".into(),
            Err(e) => format!("Cannot {action}: {e}"),
        };
        if let Err(e) = self.refresh(result.is_ok()) {
            // A partial file operation can still change the preview even if
            // rescanning the list fails. Preserve its current scroll position.
            let offsets = self.offsets;
            let columns = self.columns;
            self.select();
            self.offsets = offsets;
            self.columns = columns;
            message = format!("{message} Cannot refresh sessions: {e}");
        }
        self.message = message;
    }
    fn clamp(&mut self, width: usize, layout: Layout) {
        let lines = [logical_lines(&self.beginning), logical_lines(&self.ending)];
        self.clamp_lines(width, layout, &lines);
    }
    fn clamp_lines(&mut self, width: usize, layout: Layout, lines: &[Vec<String>; 2]) {
        self.top = self.top.min(self.entries.len().saturating_sub(layout.list));
        if self.selected < self.top {
            self.top = self.selected;
        }
        if self.selected >= self.top + layout.list {
            self.top = self.selected.saturating_sub(layout.list.saturating_sub(1));
        }
        for (i, rows) in [layout.beginning, layout.ending].into_iter().enumerate() {
            self.offsets[i] = self.offsets[i].min(lines[i].len().saturating_sub(rows.max(1)));
            self.columns[i] = self.columns[i].min(horizontal_limit(&lines[i], width));
        }
    }
    fn create_name(&mut self) {
        let mut number = 1;
        let text = loop {
            let candidate = format!("session-{number}");
            match occupied(&self.dir.join(&candidate)) {
                Ok(false) => break candidate,
                Ok(true) => number += 1,
                Err(e) => {
                    self.message = format!("Cannot suggest a name: {e}");
                    return;
                }
            }
        };
        self.naming = Some(Naming {
            cursor: text.len(),
            text,
        });
        self.message = "Edit name; Enter creates, Esc cancels.".into();
    }
    fn available(&self, name: &str) -> Result<PathBuf> {
        if inline(name) != name {
            return crate::fail("Name contains hidden or directional controls.");
        }
        if name.is_empty() || name.starts_with('.') || name.contains('/') {
            return crate::fail("Use a nonempty session name without a leading dot or slash.");
        }
        if name.len() > 107 {
            return crate::fail("Name is too long (maximum 107 bytes).");
        }
        let path = self.dir.join(name);
        storage::resolve(&path)?;
        if occupied(&path)? {
            return crate::fail("That name is already occupied; choose another.");
        }
        Ok(path)
    }
    fn key(
        &mut self,
        key: Key,
        width: usize,
        layout: Layout,
        detach: Option<u8>,
    ) -> Option<Choice> {
        if matches!(key, Key::Char('\u{3}' | '\u{4}'))
            || matches!(key, Key::Char(c) if u8::try_from(u32::from(c)).is_ok_and(|b| Some(b) == detach))
        {
            return Some(Choice::Exit);
        }
        let key = match key {
            Key::Insert if self.naming.is_none() => Key::Char('n'),
            Key::Delete if self.naming.is_none() => Key::Char('d'),
            Key::F2 if self.naming.is_none() => Key::Char('c'),
            Key::Char(c) if self.naming.is_none() => Key::Char(match c {
                'о' => 'j',
                'л' => 'k',
                'р' => 'h',
                'д' => 'l',
                'й' => 'q',
                'в' => 'd',
                'с' => 'c',
                'т' => 'n',
                _ => c,
            }),
            _ => key,
        };
        if (key == Key::Char('n') && self.naming.is_none()
            || key == Key::Enter && self.naming.is_some())
            && (layout.height < 3 || width < 6)
        {
            self.message = "Resize to show and confirm the session name.".into();
            return None;
        }
        if self.naming.is_some() {
            return self.name_key(key);
        }
        if matches!(key, Key::Char('d' | 'c'))
            && self.entries.get(self.selected).is_some()
            && (width < 25 || layout.height < 20)
        {
            self.message = "Resize before deleting or cleaning the selected session.".into();
            return None;
        }
        match key {
            Key::Escape | Key::Char('q') => return Some(Choice::Exit),
            Key::Tab | Key::BackTab => self.focus = self.focus.cycle(key == Key::BackTab),
            Key::Char('n') => self.create_name(),
            Key::Char('d') => self.manage(true),
            Key::Char('c') => self.manage(false),
            Key::Enter => {
                if let Some(entry) = self.entries.get_mut(self.selected) {
                    let path = self.dir.join(&entry.name);
                    match storage::state(&path) {
                        Ok(state) => {
                            entry.state = state;
                            match state {
                                storage::State::Running => return Some(Choice::Attach(path)),
                                storage::State::Ended | storage::State::Stale => {
                                    return Some(Choice::Restart(path));
                                }
                                storage::State::Attached => {
                                    self.message =
                                        "Session is attached; busy with another client.".into();
                                }
                                storage::State::Missing => {
                                    self.message = "Session is missing.".into();
                                }
                            }
                        }
                        Err(e) => self.message = format!("Cannot open session: {e}"),
                    }
                }
            }
            _ => self.navigate(key, width, layout),
        }
        None
    }
    fn name_key(&mut self, key: Key) -> Option<Choice> {
        if key == Key::Escape {
            self.naming = None;
            self.message.clear();
            return None;
        }
        if key == Key::Enter {
            let name = &self.naming.as_ref().expect("naming mode").text;
            match self.available(name) {
                Ok(path) => return Some(Choice::Create(path)),
                Err(e) => self.message = e.to_string(),
            }
            return None;
        }
        let name = self.naming.as_mut().expect("naming mode");
        match key {
            Key::Left => {
                name.cursor = name.text[..name.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(i, _)| i);
            }
            Key::Right => {
                name.cursor += name.text[name.cursor..]
                    .graphemes(true)
                    .next()
                    .map_or(0, str::len);
            }
            Key::Home => name.cursor = 0,
            Key::End => name.cursor = name.text.len(),
            Key::Backspace if name.cursor > 0 => {
                let previous = name.text[..name.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .expect("previous character")
                    .0;
                name.text.drain(previous..name.cursor);
                name.cursor = previous;
            }
            Key::Delete if name.cursor < name.text.len() => {
                let end = name.cursor
                    + name.text[name.cursor..]
                        .graphemes(true)
                        .next()
                        .expect("next grapheme")
                        .len();
                name.text.drain(name.cursor..end);
            }
            Key::Char(c) | Key::Paste(c) if inline(&c.to_string()) != c.to_string() => {
                self.message = "Name contains hidden or directional controls.".into();
            }
            Key::Char(c) | Key::Paste(c)
                if !c.is_control() && name.text.len() + c.len_utf8() <= 107 =>
            {
                name.text.insert(name.cursor, c);
                name.cursor += c.len_utf8();
            }
            Key::Char(c) | Key::Paste(c) if !c.is_control() => {
                self.message = "Name is too long (maximum 107 bytes).".into();
            }
            _ => {}
        }
        // Deletion can merge adjacent clusters, including regional indicators.
        name.cursor = name
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= name.cursor)
            .unwrap_or(name.text.len());
        None
    }
    fn navigate(&mut self, key: Key, width: usize, layout: Layout) {
        if matches!(key, Key::Left | Key::Right | Key::Char('h' | 'l')) {
            let (i, text) = match self.focus {
                Focus::List => return,
                Focus::Beginning => (0, &self.beginning),
                Focus::Ending => (1, &self.ending),
            };
            self.columns[i] = if matches!(key, Key::Left | Key::Char('h')) {
                self.columns[i].saturating_sub(1)
            } else {
                self.columns[i]
                    .saturating_add(1)
                    .min(horizontal_limit(&logical_lines(text), width))
            };
            return;
        }
        let (position, max, page) = match self.focus {
            Focus::List => (
                &mut self.selected,
                self.entries.len().saturating_sub(1),
                layout.list,
            ),
            Focus::Beginning => (
                &mut self.offsets[0],
                logical_lines(&self.beginning)
                    .len()
                    .saturating_sub(layout.beginning.max(1)),
                layout.beginning,
            ),
            Focus::Ending => (
                &mut self.offsets[1],
                logical_lines(&self.ending)
                    .len()
                    .saturating_sub(layout.ending.max(1)),
                layout.ending,
            ),
        };
        let previous = *position;
        *position = match key {
            Key::Up | Key::Char('k') => position.saturating_sub(1),
            Key::Down | Key::Char('j') => position.saturating_add(1).min(max),
            Key::PageUp => position.saturating_sub(page.max(1)),
            Key::PageDown => position.saturating_add(page.max(1)).min(max),
            Key::Home => 0,
            Key::End => max,
            _ => *position,
        };
        if self.focus == Focus::List && self.selected != previous {
            self.select();
        }
    }
    fn resize(&mut self, width: usize, height: usize) {
        let size = (
            u16::try_from(height).unwrap_or(200).clamp(1, 200),
            u16::try_from(width).unwrap_or(512).clamp(1, 512),
        );
        if self.virtual_size != size {
            self.virtual_size = size;
            if let Some(raw) = &self.raw[0] {
                self.beginning = emulated(raw, size, true);
            }
            if let Some(raw) = &self.raw[1] {
                self.ending = emulated(raw, size, false);
            }
        }
    }
    fn render(&mut self, width: usize, height: usize) -> Vec<u8> {
        self.resize(width, height);
        let drawable = width.saturating_sub(1);
        let inner = drawable.saturating_sub(4).max(1);
        let layout = Layout::new(height, self.focus);
        let lines = [logical_lines(&self.beginning), logical_lines(&self.ending)];
        self.clamp_lines(inner, layout, &lines);
        let mut frame = Canvas::new(drawable, height);
        if drawable >= 29 && height >= 20 {
            frame.text(1, 1, "rtch  ·  Session picker", "1;37");
            let session_rows = (self.top..self.top + layout.list)
                .map(|i| {
                    self.entries.get(i).map_or_else(String::new, |entry| {
                        session_row(entry, i == self.selected, inner)
                    })
                })
                .collect::<Vec<_>>();
            frame.pane(
                2,
                &format!(
                    "Sessions ({}/{})",
                    usize::from(!self.entries.is_empty()) + self.selected,
                    self.entries.len()
                ),
                &session_rows,
                self.focus == Focus::List,
                Some(self.selected.saturating_sub(self.top)),
            );
            let mut row = 4 + layout.list;
            for (i, (title, count, focus)) in [
                ("Beginning", layout.beginning, Focus::Beginning),
                ("Ending", layout.ending, Focus::Ending),
            ]
            .into_iter()
            .enumerate()
            {
                let visible = (self.offsets[i]..self.offsets[i] + count)
                    .map(|n| {
                        lines[i].get(n).map_or_else(String::new, |line| {
                            clip_window(line, self.columns[i], inner)
                        })
                    })
                    .collect::<Vec<_>>();
                frame.pane(
                    row,
                    &preview_title(title, self.offsets[i], self.columns[i], &lines[i], inner),
                    &visible,
                    self.focus == focus,
                    None,
                );
                row += count + 2;
            }
        } else {
            if height > 3 {
                frame.text(1, 1, "rtch  ·  Session picker", "1;37");
            }
            if height > 4 {
                frame.text(2, 1, "Resize for session previews", "1;33");
            }
            if height > 5
                && let Some(entry) = self.entries.get(self.selected)
            {
                frame.text(3, 1, &session_row(entry, true, drawable), "7");
            }
        }
        if height >= 3 {
            if let Some(name) = &self.naming {
                let (text, cursor) = name_window(name, drawable.saturating_sub(5));
                frame.text(height - 2, 1, &format!("New: {text}"), "1;36");
                frame.cursor = Some((height - 2, (6 + cursor).min(drawable.max(1))));
            } else {
                frame.text(height - 2, 1, action_hint(drawable, height), "2;37");
            }
        }
        if height >= 2 {
            frame.text(height - 1, 1, &self.message, "1;33");
        }
        frame.text(
            height.max(1),
            1,
            if height < 3 && self.naming.is_some() {
                "Esc cancel · resize"
            } else if height < 3 {
                "Esc resize"
            } else if drawable < 30 {
                "Esc exit"
            } else if drawable < 40 {
                "Esc exit  Tab  arrows scroll"
            } else if drawable < 64 {
                "Esc exit  Tab focus  ↑/↓ ←/→ scroll"
            } else {
                "Esc exit  Tab/Shift+Tab focus  ↑/↓ ←/→ PgUp/PgDn Home/End scroll"
            },
            "1;37",
        );
        frame.finish()
    }
}

fn action_hint(width: usize, height: usize) -> &'static str {
    if width < 10 {
        match width {
            0 => "",
            1..=5 => "↔",
            _ => "Resize",
        }
    } else if height >= 20 && width >= 29 {
        let full = "Enter opens · Insert new · Delete ended/stale · F2 clean logs";
        if display_width(full) <= width {
            full
        } else {
            "Enter Ins:new Del F2:clear"
        }
    } else if display_width("Enter opens · Insert new") <= width {
        "Enter opens · Insert new"
    } else if width >= 18 {
        "Enter open Ins new"
    } else {
        "Ins new"
    }
}

struct Canvas {
    buffer: Buffer,
    cursor: Option<(usize, usize)>,
}
fn ui_style(code: &str) -> Style {
    match code {
        "1;36" => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        "2;37" => Style::new().fg(Color::White).add_modifier(Modifier::DIM),
        "1;37" => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        "1;33" => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        "7" => Style::new().add_modifier(Modifier::REVERSED),
        _ => Style::new(),
    }
}
impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            buffer: Buffer::empty(Rect::new(
                0,
                0,
                u16::try_from(width).unwrap_or(511),
                u16::try_from(height).unwrap_or(200),
            )),
            cursor: None,
        }
    }
    fn text(&mut self, row: usize, column: usize, text: &str, style: &str) {
        let width = usize::from(self.buffer.area.width);
        if row == 0 || row > usize::from(self.buffer.area.height) || column == 0 || column > width {
            return;
        }
        let y = u16::try_from(row - 1).expect("bounded row");
        let mut x = u16::try_from(column - 1).expect("bounded column");
        for grapheme in clip(text, width - column + 1).graphemes(true) {
            let size = display_width(grapheme);
            if size == 0 {
                continue;
            }
            self.buffer[(x, y)]
                .set_symbol(grapheme)
                .set_style(ui_style(style))
                .set_diff_option(CellDiffOption::ForcedWidth(
                    std::num::NonZeroU16::new(u16::try_from(size).expect("bounded grapheme"))
                        .expect("positive width"),
                ));
            for extra in 1..size {
                self.buffer[(x + u16::try_from(extra).expect("bounded cell"), y)]
                    .set_diff_option(CellDiffOption::Skip);
            }
            x += u16::try_from(size).expect("bounded grapheme");
        }
    }
    fn pane(
        &mut self,
        top: usize,
        title: &str,
        rows: &[String],
        focused: bool,
        selected: Option<usize>,
    ) {
        let width = usize::from(self.buffer.area.width);
        let area = Rect::new(
            0,
            u16::try_from(top - 1).expect("bounded top"),
            self.buffer.area.width,
            u16::try_from(rows.len() + 2).expect("bounded height"),
        );
        let style = ui_style(if focused { "1;36" } else { "2;37" });
        Block::new()
            .borders(Borders::ALL)
            .title(format!(" {}{title} ", if focused { "● " } else { "" }))
            .border_style(style)
            .title_style(style)
            .render(area, &mut self.buffer);
        let inner = width.saturating_sub(4);
        for (i, line) in rows.iter().enumerate() {
            let text = clip(line, inner);
            self.text(
                top + i + 1,
                2,
                &format!(
                    " {text}{} ",
                    " ".repeat(inner.saturating_sub(display_width(&text)))
                ),
                if selected == Some(i) { "7" } else { "0" },
            );
        }
    }
    fn finish(self) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut output = String::from("\x1b[?25l");
        let mut previous = None;
        for y in 0..self.buffer.area.height {
            write!(output, "\x1b[{};1H\x1b[2K", y + 1).expect("string write");
            for x in 0..self.buffer.area.width {
                let cell = &self.buffer[(x, y)];
                if cell.diff_option == CellDiffOption::Skip {
                    continue;
                }
                let style = (cell.fg, cell.modifier);
                if previous != Some(style) {
                    let fg = match cell.fg {
                        Color::Cyan => 36,
                        Color::White => 37,
                        Color::Yellow => 33,
                        _ => 39,
                    };
                    write!(
                        output,
                        "\x1b[0;{fg}{}{}{}m",
                        if cell.modifier.contains(Modifier::BOLD) {
                            ";1"
                        } else {
                            ""
                        },
                        if cell.modifier.contains(Modifier::DIM) {
                            ";2"
                        } else {
                            ""
                        },
                        if cell.modifier.contains(Modifier::REVERSED) {
                            ";7"
                        } else {
                            ""
                        }
                    )
                    .expect("string write");
                    previous = Some(style);
                }
                output.push_str(cell.symbol());
                if char_width(cell.symbol()) != cell.symbol().width() {
                    write!(
                        output,
                        "\x1b[{};{}H",
                        y + 1,
                        usize::from(x) + display_width(cell.symbol()) + 1
                    )
                    .expect("string write");
                }
            }
        }
        output.push_str("\x1b[0m");
        if let Some((row, column)) = self.cursor {
            write!(output, "\x1b[{row};{column}H\x1b[?25h").expect("string write");
        }
        output.into_bytes()
    }
}

fn inline(text: &str) -> String {
    history::preview(text.as_bytes()).replace(['\n', '\t'], " ")
}
fn display_width(text: &str) -> usize {
    text.graphemes(true)
        .map(|grapheme| char_width(grapheme).max(grapheme.width()))
        .sum()
}
fn char_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}
fn clip(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for grapheme in inline(text).graphemes(true) {
        let size = display_width(grapheme);
        if used + size > width {
            break;
        }
        used += size;
        result.push_str(grapheme);
    }
    result
}
fn elide(text: &str, width: usize) -> String {
    let text = inline(text);
    if display_width(&text) <= width {
        return text;
    }
    if width < 3 {
        return clip("…", width);
    }
    let tail_width = (width / 3).min(8);
    let mut tail = Vec::new();
    let mut used = 0;
    for grapheme in text.graphemes(true).rev() {
        let size = display_width(grapheme);
        if used + size > tail_width {
            break;
        }
        tail.push(grapheme);
        used += size;
    }
    format!(
        "{}…{}",
        clip(&text, width - used - 1),
        tail.into_iter().rev().collect::<String>()
    )
}
fn session_row(entry: &Entry, selected: bool, width: usize) -> String {
    let state = format!("[{}]", label(entry.state));
    let name_width = width.saturating_sub(state.len() + 3);
    let name = elide(&entry.name, name_width);
    format!(
        "{} {}{} {state}",
        if selected { ">" } else { " " },
        name,
        " ".repeat(name_width.saturating_sub(display_width(&name)))
    )
}
fn name_window(name: &Naming, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let graphemes = name.text.grapheme_indices(true).collect::<Vec<_>>();
    let cursor = graphemes
        .iter()
        .position(|(i, _)| *i >= name.cursor)
        .unwrap_or(graphemes.len());
    let mut start = cursor;
    let mut before = 0;
    while start > 0 {
        let size = display_width(&inline(graphemes[start - 1].1));
        if before + size > width.saturating_sub(3) {
            break;
        }
        before += size;
        start -= 1;
    }
    let mut text = if start > 0 {
        String::from("‹")
    } else {
        String::new()
    };
    let cursor_column = usize::from(start > 0) + before;
    let mut used = usize::from(start > 0);
    for (_, grapheme) in &graphemes[start..] {
        let grapheme = inline(grapheme);
        let size = display_width(&grapheme);
        if used + size > width.saturating_sub(1) {
            text.push('›');
            break;
        }
        text.push_str(&grapheme);
        used += size;
    }
    (text, cursor_column.min(width.saturating_sub(1)))
}
fn logical_lines(text: &str) -> Vec<String> {
    let mut rows = text.lines().map(str::to_owned).collect::<Vec<_>>();
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}
fn horizontal_limit(lines: &[String], width: usize) -> usize {
    lines
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0)
        .saturating_sub(width)
}
fn preview_title(title: &str, row: usize, column: usize, lines: &[String], width: usize) -> String {
    let title = format!("{title} ({}/{})", row + 1, lines.len());
    let limit = horizontal_limit(lines, width);
    if limit > 0 {
        format!("{title} · col {}/{}", column + 1, limit + width)
    } else {
        title
    }
}
fn clip_window(text: &str, offset: usize, width: usize) -> String {
    let mut result = String::new();
    let mut column = 0;
    let end = offset.saturating_add(width);
    for grapheme in inline(text).graphemes(true) {
        let next = column + display_width(grapheme);
        if column >= end {
            break;
        }
        if next > offset {
            if column >= offset && next <= end {
                result.push_str(grapheme);
            } else {
                // Keep the columns of a partially visible wide grapheme blank.
                result.push_str(&" ".repeat(next.min(end) - column.max(offset)));
            }
        }
        column = next;
    }
    result
}

struct Terminal {
    original: libc::termios,
    flags: i32,
    screen: bool,
}
impl Terminal {
    fn enter() -> Result<Self> {
        let mut guard = Self {
            original: os::term(0)?,
            flags: os::flags(1)?,
            screen: false,
        };
        os::nonblocking(1)?;
        os::set_term(0, &os::raw(guard.original), false)?;
        guard.screen = true;
        Self::write(b"\x18\x1b\\\x1b[?1049h\x1b[r\x1b[?6l\x1b[?25l\x1b[?2004h\x1b[0m\x1b[2J")?;
        Ok(guard)
    }
    fn write(mut bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !bytes.is_empty() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal output stalled",
                ));
            }
            let mut fds = [libc::pollfd {
                fd: 1,
                events: libc::POLLOUT,
                revents: 0,
            }];
            os::poll(&mut fds, 20)?;
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("terminal output disconnected"));
            }
            if fds[0].revents & libc::POLLOUT != 0 {
                match os::write(1, bytes) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => bytes = &bytes[n..],
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if self.screen {
            let _ = Self::write(b"\x1b[?2004l\x1b[0m\x1b[?25h\x1b[?1049l");
        }
        let _ = os::set_term(0, &self.original, true);
        let _ = os::set_flags(1, self.flags);
    }
}

pub fn pick(o: &Options, mut open: impl FnMut(Choice, &mut bool) -> Result<()>) -> Result<()> {
    if !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || std::env::var("TERM").is_ok_and(|term| term == "dumb")
    {
        println!(
            "rtch: session picker requires an interactive terminal; use rtch list, rtch attach NAME, or rtch new NAME."
        );
        return Ok(());
    }
    let mut model = Model::load()?;
    loop {
        let choice = choose(o, &mut model)?;
        if matches!(choice, Choice::Exit) {
            return Ok(());
        }
        let mut established = false;
        match open(choice, &mut established) {
            Ok(()) => return Ok(()),
            Err(error) if established => return Err(error),
            Err(error) => model.message = format!("Cannot open session: {error}"),
        }
    }
}

fn choose(o: &Options, model: &mut Model) -> Result<Choice> {
    let _signals = os::picker_signals()?;
    let _terminal = Terminal::enter()?;
    let mut decoder = Decoder::default();
    let mut dirty = true;
    loop {
        let signals = os::take_signals();
        if signals & 1 != 0 {
            return Ok(Choice::Exit);
        }
        dirty |= signals & 2 != 0;
        let size = os::size(0);
        let width = usize::from(size.ws_col).clamp(1, 512);
        let height = usize::from(size.ws_row).clamp(1, 200);
        if dirty {
            Terminal::write(&model.render(width, height))?;
            dirty = false;
        }
        let mut fds = [libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        }];
        os::poll(&mut fds, 20)?;
        let mut keys = Vec::new();
        if fds[0].revents & libc::POLLIN != 0 {
            let mut bytes = [0; 256];
            match os::read(0, &mut bytes) {
                Ok(0) => return Ok(Choice::Exit),
                Ok(n) => {
                    if bytes[..n]
                        .iter()
                        .any(|b| matches!(b, 3 | 4) || b.is_ascii_control() && Some(*b) == o.detach)
                    {
                        return Ok(Choice::Exit);
                    }
                    keys = decoder.feed(&bytes[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(Choice::Exit);
        }
        keys.extend(decoder.expire());
        for key in keys {
            let layout = Layout::new(height, model.focus);
            let inner = width.saturating_sub(5).max(1);
            model.clamp(inner, layout);
            if let Some(choice) = model.key(key, inner, layout, o.detach) {
                return Ok(choice);
            }
            dirty = true;
        }
    }
}

fn occupied(path: &std::path::Path) -> io::Result<bool> {
    for candidate in [
        path.to_owned(),
        storage::side(path, ".log"),
        storage::side(path, ".ended"),
        storage::side(path, ".head"),
    ] {
        match fs::symlink_metadata(candidate) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

pub fn shell_init() -> Result<()> {
    io::stdout().write_all(SHELL_INIT.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    mod screen {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/screen.rs"
        ));
    }
    fn model() -> Model {
        Model {
            dir: std::env::temp_dir(),
            entries: (0..10)
                .map(|i| Entry {
                    name: format!("item-{i}"),
                    state: storage::State::Ended,
                    activity: SystemTime::UNIX_EPOCH,
                })
                .collect(),
            selected: 0,
            top: 0,
            focus: Focus::List,
            beginning: "begin\n".repeat(60),
            ending: "end\n".repeat(60),
            raw: [None, None],
            virtual_size: (24, 80),
            offsets: [0, usize::MAX],
            columns: [0, 0],
            naming: None,
            message: String::new(),
        }
    }
    #[test]
    fn fragmented_keys_unicode_and_escape_timeout() {
        let input = "\x1b[A\x1b[6~\x1b[Z\x1bOH\x1b[2~\x1b[3~\x1bOQ\x1b[12~\x1b[1Q\x1b[1;1Q\tЖ";
        let expected = vec![
            Key::Up,
            Key::PageDown,
            Key::BackTab,
            Key::Home,
            Key::Insert,
            Key::Delete,
            Key::F2,
            Key::F2,
            Key::F2,
            Key::F2,
            Key::Tab,
            Key::Char('Ж'),
        ];
        for n in 1..=input.len() {
            let mut decoder = Decoder::default();
            let keys = input
                .as_bytes()
                .chunks(n)
                .flat_map(|bytes| decoder.feed(bytes))
                .collect::<Vec<_>>();
            assert_eq!(keys, expected);
        }
        let mut decoder = Decoder::default();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(decoder.expire(), None);
        decoder.since = Instant::now().checked_sub(Duration::from_millis(100));
        assert_eq!(decoder.expire(), Some(Key::Escape));
        decoder.feed(b"\x1b[");
        decoder.since = Instant::now().checked_sub(Duration::from_millis(100));
        assert_eq!(decoder.expire(), None);
        assert_eq!(decoder.pending, b"\x1b[");
        assert_eq!(decoder.feed(b"A"), [Key::Up]);
        assert!(decoder.feed(&[0xd0]).is_empty());
        decoder.since = Instant::now().checked_sub(Duration::from_secs(1));
        assert_eq!(decoder.expire(), None);
        assert_eq!(decoder.feed(&[0x96]), [Key::Char('Ж')]);
    }
    #[test]
    fn management_keys_require_complete_unmodified_sequences_and_correct_prefixes() {
        for (bytes, key) in [
            (&b"\x1b[2~"[..], Key::Insert),
            (&b"\x1b[3~"[..], Key::Delete),
            (&b"\x1bOQ"[..], Key::F2),
            (&b"\x1b[12~"[..], Key::F2),
            (&b"\x1b[1Q"[..], Key::F2),
            (&b"\x1b[1;1Q"[..], Key::F2),
        ] {
            for split in 1..bytes.len() {
                let mut decoder = Decoder::default();
                assert!(decoder.feed(&bytes[..split]).is_empty());
                if split > 1 {
                    decoder.since = Instant::now().checked_sub(Duration::from_secs(1));
                }
                assert_eq!(decoder.expire(), None);
                assert_eq!(decoder.feed(&bytes[split..]), [key]);
                assert!(decoder.pending.is_empty());
            }
        }
        for bytes in [
            &b"\x1bO2~"[..],
            &b"\x1bO3~"[..],
            &b"\x1bO12~"[..],
            &b"\x1bO1Q"[..],
            &b"\x1bO1;1Q"[..],
            &b"\x1b[Q"[..],
            &b"\x1b[2;1~"[..],
            &b"\x1b[2;2~"[..],
            &b"\x1b[3;1~"[..],
            &b"\x1b[3;2~"[..],
            &b"\x1b[12;1~"[..],
            &b"\x1b[12;2~"[..],
            &b"\x1b[1;2Q"[..],
            &b"\x1b[2Q"[..],
            &b"\x1b[0Q"[..],
            &b"\x1b[1;1;1Q"[..],
            &b"\x1b[1:1Q"[..],
        ] {
            for chunk in 1..=bytes.len() {
                let mut decoder = Decoder::default();
                for part in bytes.chunks(chunk) {
                    assert!(decoder.feed(part).is_empty(), "{bytes:?}");
                }
                assert_eq!(decoder.feed(b"\x1b[2~"), [Key::Insert]);
            }
        }
        for suffix in ["2~", "3~", "12~", "1Q"] {
            let bytes = format!("\x1b[{}{suffix}", "1".repeat(40));
            let mut decoder = Decoder::default();
            for byte in bytes.bytes() {
                assert!(decoder.feed(&[byte]).is_empty());
            }
            assert_eq!(decoder.feed(b"\x1bOQ"), [Key::F2]);
        }
    }
    #[test]
    fn focus_and_scroll_offsets_are_independent_and_resize_clamps() {
        let mut m = model();
        let rendered = String::from_utf8(m.render(80, 24)).unwrap();
        assert_eq!(rendered.matches("item-").count(), 8);
        assert_eq!(m.offsets, [0, 57]);
        m.key(Key::Tab, 80, Layout::new(24, m.focus), Some(28));
        let rendered = String::from_utf8(m.render(80, 24)).unwrap();
        assert_eq!(m.focus, Focus::Beginning);
        assert!(rendered.contains("Sessions") && rendered.contains("Ending"));
        let ending_offset = m.offsets[1];
        m.key(Key::PageDown, 80, Layout::new(24, m.focus), Some(28));
        assert_eq!(m.offsets, [12, ending_offset]);
        m.key(Key::Tab, 80, Layout::new(24, m.focus), Some(28));
        m.render(80, 24);
        let beginning_offset = m.offsets[0];
        m.key(Key::Home, 80, Layout::new(24, m.focus), Some(28));
        assert_eq!(m.offsets, [beginning_offset, 0]);
        m.key(Key::BackTab, 80, Layout::new(24, m.focus), Some(28));
        assert_eq!(m.focus, Focus::Beginning);
        m.offsets = [usize::MAX, usize::MAX];
        m.render(80, 100);
        assert_eq!(m.offsets[0], 0);
        assert!(m.offsets[1] < 60);
    }
    #[test]
    fn english_and_russian_keys_match_arrows_in_each_pane_and_remain_literal_in_names() {
        for keys in [['j', 'k', 'h', 'l'], ['о', 'л', 'р', 'д']] {
            for focus in [Focus::List, Focus::Beginning, Focus::Ending] {
                let mut shortcuts = model();
                let mut arrows = model();
                for m in [&mut shortcuts, &mut arrows] {
                    m.focus = focus;
                    m.selected = 3;
                    m.beginning = format!("{}\n", "x".repeat(100)).repeat(60);
                    m.ending = m.beginning.clone();
                    m.offsets = [10, 20];
                    m.columns = [10, 20];
                }
                for (key, arrow) in
                    keys.into_iter()
                        .zip([Key::Down, Key::Up, Key::Left, Key::Right])
                {
                    shortcuts.key(Key::Char(key), 80, Layout::new(24, focus), Some(28));
                    arrows.key(arrow, 80, Layout::new(24, focus), Some(28));
                    assert_eq!(shortcuts.selected, arrows.selected);
                    assert_eq!(shortcuts.offsets, arrows.offsets);
                    assert_eq!(shortcuts.columns, arrows.columns);
                    assert_eq!(shortcuts.beginning, arrows.beginning);
                    assert_eq!(shortcuts.ending, arrows.ending);
                }
            }
        }
        let mut m = model();
        m.naming = Some(Naming {
            text: String::new(),
            cursor: 0,
        });
        let name = "jkhlqdcnолрдйвст";
        for key in name.chars() {
            assert!(
                m.key(Key::Char(key), 80, Layout::new(24, m.focus), Some(28))
                    .is_none()
            );
        }
        assert_eq!(m.naming.as_ref().unwrap().text, name);
        assert_eq!(m.selected, 0);
        assert_eq!(m.offsets, [0, usize::MAX]);
    }
    #[test]
    fn russian_shortcuts_wait_for_complete_utf8_and_do_not_add_shifted_bindings() {
        let mut m = model();
        let mut decoder = Decoder::default();
        for expected in [1, 2] {
            let mut bytes = [0; 4];
            let input = 'о'.encode_utf8(&mut bytes).as_bytes();
            assert!(decoder.feed(&input[..1]).is_empty());
            assert_eq!(m.selected, expected - 1);
            let keys = decoder.feed(&input[1..]);
            assert_eq!(keys, [Key::Char('о')]);
            for key in keys {
                assert!(m.key(key, 80, Layout::new(24, m.focus), None).is_none());
            }
            assert_eq!(m.selected, expected);
        }
        m.offsets = [10, 20];
        m.columns = [5, 7];
        for focus in [Focus::List, Focus::Beginning, Focus::Ending] {
            m.focus = focus;
            for c in "JKHLQDCNОЛРДЙВСТф".chars() {
                assert!(
                    m.key(Key::Char(c), 80, Layout::new(24, focus), None)
                        .is_none()
                );
                assert_eq!(m.selected, 2);
                assert_eq!(m.offsets, [10, 20]);
                assert_eq!(m.columns, [5, 7]);
                assert!(m.naming.is_none());
                assert!(m.message.is_empty());
            }
        }
    }
    #[test]
    fn original_detach_character_takes_priority_over_shortcuts() {
        let mut m = model();
        m.dir = std::env::temp_dir().join(format!("rtch-picker-detach-{}", std::process::id()));
        assert!(matches!(
            m.key(Key::Char('n'), 80, Layout::new(24, m.focus), Some(b'n')),
            Some(Choice::Exit)
        ));
        assert!(m.naming.is_none());
        assert!(
            m.key(Key::Char('т'), 80, Layout::new(24, m.focus), Some(b'n'))
                .is_none()
        );
        assert!(m.naming.is_some());
        assert!(matches!(
            m.key(Key::Char('n'), 80, Layout::new(24, m.focus), Some(b'n')),
            Some(Choice::Exit)
        ));
    }
    #[test]
    fn missing_enter_stays_in_picker_and_small_terminal_keeps_controls() {
        let mut m = model();
        assert!(
            m.key(Key::Enter, 80, Layout::new(24, m.focus), Some(28))
                .is_none()
        );
        assert!(m.message.contains("missing"), "{}", m.message);
        let rendered = String::from_utf8(m.render(20, 6)).unwrap();
        assert!(rendered.contains("Resize") && rendered.contains("Ins new"));
        assert!(rendered.contains("Esc exit"));
        let rendered = String::from_utf8(m.render(8, 1)).unwrap();
        assert!(rendered.contains("Esc"));
    }
    #[test]
    fn unicode_window_clipping_blanks_partial_graphemes_at_both_edges() {
        assert_eq!(logical_lines("a界e\u{301}👩‍💻Z"), ["a界e\u{301}👩‍💻Z"]);
        assert_eq!(clip("界e\u{301}👩‍💻", 3), "界e\u{301}");
        assert_eq!(clip("hi\x1b[31m\n\t\u{202e}bye", 80), "hi  bye");
        let text = "a界e\u{301}👩‍💻Z";
        assert_eq!(clip_window(text, 0, 2), "a ");
        assert_eq!(clip_window(text, 2, 3), " e\u{301} ");
        assert_eq!(clip_window(text, 5, 4), "   Z");
        assert_eq!(clip_window(text, 3, 5), "e\u{301}👩‍💻");
        for width in 0..10 {
            for offset in 0..12 {
                assert!(display_width(&clip_window(text, offset, width)) <= width);
            }
        }
    }
    #[test]
    fn horizontal_offsets_are_independent_clamped_and_reset_by_selection() {
        let mut m = model();
        m.beginning = format!("{}SUFFIX\nsecond", "x".repeat(100));
        m.ending = format!("{}ENDING\nsecond", "0123456789".repeat(12));
        let initial = m.selected;
        for key in [Key::Left, Key::Right, Key::Char('h'), Key::Char('l')] {
            m.key(key, 75, Layout::new(24, m.focus), None);
        }
        assert_eq!(m.selected, initial);
        assert_eq!(m.columns, [0, 0]);
        m.focus = Focus::Beginning;
        m.render(80, 24);
        for _ in 0..40 {
            m.key(Key::Char('l'), 75, Layout::new(24, m.focus), None);
        }
        assert_eq!(m.columns, [31, 0]);
        assert_eq!(m.offsets, [0, 1]);
        let mut host = screen::Screen::new(80, 24);
        host.feed(&m.render(80, 24));
        host.assert_spare_column();
        assert!(host.row(6).contains("SUFFIX"));
        assert!(host.row(7).contains("│") && !host.row(7).contains("SUFFIX"));
        m.key(Key::Tab, 75, Layout::new(24, m.focus), None);
        m.render(80, 24);
        m.key(Key::Home, 75, Layout::new(24, m.focus), None);
        for _ in 0..15 {
            m.key(Key::Right, 75, Layout::new(24, m.focus), None);
        }
        assert_eq!(m.columns, [31, 15]);
        host.feed(&m.render(80, 24));
        host.assert_spare_column();
        assert!(host.row(6).contains("SUFFIX"));
        assert!(host.row(5).contains("col 32/106"));
        assert!(host.row(8).contains("col 16/126"));
        assert!(
            host.row(9)
                .contains(&clip_window(m.ending.lines().next().unwrap(), 15, 75))
        );
        m.key(Key::Left, 75, Layout::new(24, m.focus), None);
        assert_eq!(m.columns, [31, 14]);
        m.key(Key::BackTab, 75, Layout::new(24, m.focus), None);
        m.key(Key::Char('h'), 75, Layout::new(24, m.focus), None);
        assert_eq!(m.columns, [30, 14]);
        m.render(110, 24);
        assert_eq!(m.columns, [1, 14]);
        m.render(196, 95);
        assert_eq!(m.columns, [0, 0]);
        m.focus = Focus::List;
        m.columns = [10, 20];
        m.key(Key::Down, 75, Layout::new(24, m.focus), None);
        assert_eq!(m.columns, [0, 0]);
        assert_eq!(m.offsets, [0, usize::MAX]);
    }
    #[test]
    fn browsing_paste_never_runs_shortcuts_and_empty_actions_are_noops() {
        let mut m = model();
        m.entries.clear();
        m.select();
        for (width, height) in [(75, 24), (5, 1)] {
            for key in [
                Key::Delete,
                Key::F2,
                Key::Char('d'),
                Key::Char('c'),
                Key::Char('в'),
                Key::Char('с'),
            ] {
                assert!(
                    m.key(key, width, Layout::new(height, m.focus), None)
                        .is_none()
                );
                assert!(m.message.is_empty());
            }
        }
        let input =
            "\x1b[200~jkhlqdcnолрдйвст\x1b[2~\x1b[3~\x1bOQ\x1b[12~\x1b[1Q\x1b[1;1Q\x1b[201~"
                .as_bytes();
        for chunk in 1..=input.len() {
            let mut decoder = Decoder::default();
            for part in input.chunks(chunk) {
                for key in decoder.feed(part) {
                    assert!(m.key(key, 75, Layout::new(24, m.focus), None).is_none());
                }
            }
            assert!(m.naming.is_none());
            assert_eq!(m.columns, [0, 0]);
            assert!(m.message.is_empty());
        }
        for c in ['q', 'й'] {
            assert!(matches!(
                m.key(Key::Char(c), 75, Layout::new(24, m.focus), None),
                Some(Choice::Exit)
            ));
        }
    }
    #[test]
    fn standard_keys_keep_name_editing_and_original_detach_priority() {
        let mut m = model();
        m.dir = std::env::temp_dir().join(format!("rtch-picker-standard-{}", std::process::id()));
        assert!(
            m.key(Key::Insert, 75, Layout::new(24, m.focus), Some(b'n'))
                .is_none()
        );
        assert_eq!(m.naming.as_ref().unwrap().text, "session-1");
        m.naming = Some(Naming {
            text: "界e\u{301}👩‍💻jkhlqdcnолрдйвст".into(),
            cursor: 3,
        });
        for key in [Key::Insert, Key::F2] {
            assert!(m.key(key, 75, Layout::new(24, m.focus), None).is_none());
            assert_eq!(
                m.naming.as_ref().unwrap().text,
                "界e\u{301}👩‍💻jkhlqdcnолрдйвст"
            );
            assert_eq!(m.naming.as_ref().unwrap().cursor, 3);
        }
        assert!(
            m.key(Key::Delete, 75, Layout::new(24, m.focus), None)
                .is_none()
        );
        assert_eq!(m.naming.as_ref().unwrap().text, "界👩‍💻jkhlqdcnолрдйвст");
        assert!(
            m.key(Key::Delete, 75, Layout::new(24, m.focus), None)
                .is_none()
        );
        assert_eq!(m.naming.as_ref().unwrap().text, "界jkhlqdcnолрдйвст");
        assert_eq!(m.entries.len(), 10);
        m.key(Key::Escape, 75, Layout::new(24, m.focus), None);
        assert!(m.naming.is_none());
        m.entries.clear();
        for (key, detach) in [(Key::Delete, b'd'), (Key::F2, b'c')] {
            assert!(
                m.key(key, 75, Layout::new(24, m.focus), Some(detach))
                    .is_none()
            );
        }
    }
    #[test]
    fn primary_hints_fit_normal_and_small_layouts_and_management_guards() {
        let mut m = model();
        for (width, height) in [
            (80, 24),
            (196, 95),
            (30, 20),
            (31, 20),
            (40, 20),
            (20, 6),
            (12, 3),
            (11, 3),
            (10, 3),
            (8, 3),
            (8, 1),
        ] {
            let mut screen = screen::Screen::new(width, height);
            screen.feed(&m.render(width, height));
            screen.assert_spare_column();
            assert!(screen.row(height).contains("Esc"));
            if height >= 3 {
                let hint = screen.row(height - 2);
                let drawable = width - 1;
                let chosen = action_hint(drawable, height);
                assert!(display_width(chosen) <= drawable);
                assert_eq!(hint.trim_end(), chosen);
                if width < 11 {
                    assert_eq!(chosen, "Resize");
                } else {
                    assert!(
                        hint.contains("Insert new")
                            || hint.contains("Ins new")
                            || hint.contains("Ins:new")
                    );
                }
                if height >= 20 && width >= 30 {
                    assert!(hint.contains("Enter") && hint.contains("Del"));
                    assert!(hint.contains("F2 clean logs") || hint.contains("F2:clear"));
                } else {
                    assert!(!hint.contains("Del") && !hint.contains("F2"));
                }
            }
        }
        for width in 0..10 {
            let hint = action_hint(width, 3);
            assert!(display_width(hint) <= width);
            assert!(!hint.contains("Ins"));
        }
        for width in [8_usize, 10] {
            assert!(
                m.key(Key::Insert, width - 5, Layout::new(3, m.focus), None)
                    .is_none()
            );
            assert!(m.message.contains("Resize"));
            assert!(m.naming.is_none());
        }
        for (width, height) in [(75, 19), (24, 24)] {
            for key in [Key::Delete, Key::F2] {
                assert!(
                    m.key(key, width, Layout::new(height, m.focus), None)
                        .is_none()
                );
                assert!(m.message.contains("Resize before"));
                assert_eq!(m.entries.len(), 10);
            }
        }
    }
    #[test]
    fn naming_errors_cancel_and_unicode_editing() {
        let mut m = model();
        m.naming = Some(Naming {
            text: "bad.head".into(),
            cursor: 8,
        });
        assert!(m.name_key(Key::Enter).is_none());
        assert!(m.message.contains("invalid session"));
        m.naming = Some(Naming {
            text: "Жa".into(),
            cursor: 2,
        });
        m.name_key(Key::Backspace);
        assert_eq!(m.naming.as_ref().unwrap().text, "a");
        m.name_key(Key::Char('界'));
        assert_eq!(m.naming.as_ref().unwrap().text, "界a");
        m.name_key(Key::Escape);
        assert!(m.naming.is_none());
        assert!(matches!(
            m.key(Key::Escape, 80, Layout::new(24, m.focus), Some(28)),
            Some(Choice::Exit)
        ));
    }
    #[test]
    fn suggestion_surfaces_filesystem_errors() {
        let mut m = model();
        m.dir = PathBuf::from("/dev/null");
        m.create_name();
        assert!(m.naming.is_none());
        assert!(m.message.contains("Cannot suggest a name"));
    }
    #[test]
    fn framed_coordinates_unicode_focus_resize_and_state_labels() {
        let mut m = model();
        m.beginning = format!("{}👩‍💻\n{}\n", "x".repeat(74), "界".repeat(100));
        m.ending = m.beginning.clone();
        m.entries[0].name = format!("{}FIRST", "界".repeat(40));
        m.entries[1].name = format!("{}SECOND", "界".repeat(40));
        for focus in [Focus::List, Focus::Beginning, Focus::Ending] {
            m.focus = focus;
            let bytes = m.render(80, 24);
            assert!(!bytes.contains(&b'\n') && !bytes.contains(&b'\r'));
            let mut screen = screen::Screen::new(80, 24);
            screen.feed(&bytes);
            screen.assert_spare_column();
            assert!(screen.row(2).starts_with('┌') && screen.row(2).contains("Sessions"));
            assert!(screen.rows().iter().any(|row| row.contains("Beginning")));
            assert!(screen.rows().iter().any(|row| row.contains("Ending")));
            if focus == Focus::List {
                assert_eq!(
                    screen.rows()[2..10]
                        .iter()
                        .filter(|row| row.starts_with('│'))
                        .count(),
                    8
                );
                assert!(
                    screen.row(3).contains('…')
                        && screen.row(3).contains("FIRST")
                        && screen.row(3).contains("[ended]")
                );
                assert!(screen.row(4).contains("SECOND"));
                assert!(screen.row(12).starts_with('┌') && screen.row(16).starts_with('└'));
                assert!(screen.row(17).starts_with('┌') && screen.row(21).starts_with('└'));
            }
            assert!(
                std::str::from_utf8(&bytes)
                    .unwrap()
                    .contains("\x1b[0;36;1m")
            );
        }
        for (width, height) in [(20, 6), (80, 24), (40, 20), (8, 1)] {
            let mut screen = screen::Screen::new(width, height);
            screen.feed(&m.render(width, height));
            screen.assert_spare_column();
            assert!(screen.row(height).contains("Esc"));
        }
        assert_eq!(display_width("👩‍💻"), 4);
        assert_eq!(
            clip_window(&format!("{}👩‍💻", "x".repeat(74)), 0, 75),
            format!("{} ", "x".repeat(74))
        );
    }
    #[test]
    fn naming_viewport_graphemes_limits_and_separate_error_row() {
        let mut m = model();
        let text = format!("{}👩‍💻e\u{301}", "x".repeat(70));
        m.naming = Some(Naming {
            cursor: text.len(),
            text: text.clone(),
        });
        m.message = "That name is already occupied.".into();
        let bytes = m.render(40, 20);
        let mut screen = screen::Screen::new(40, 20);
        screen.feed(&bytes);
        screen.assert_spare_column();
        assert!(screen.row(18).contains("New: ‹") && screen.row(18).contains("e\u{301}"));
        assert!(screen.row(19).contains("already occupied"));
        assert!(std::str::from_utf8(&bytes).unwrap().ends_with("\x1b[?25h"));
        m.name_key(Key::Backspace);
        assert_eq!(
            m.naming.as_ref().unwrap().text,
            format!("{}👩‍💻", "x".repeat(70))
        );
        m.name_key(Key::Left);
        assert_eq!(m.naming.as_ref().unwrap().cursor, 70);
        m.name_key(Key::Delete);
        assert_eq!(m.naming.as_ref().unwrap().text, "x".repeat(70));
        m.naming = Some(Naming {
            text: "z".repeat(108),
            cursor: 108,
        });
        assert!(m.name_key(Key::Enter).is_none());
        assert!(m.message.contains("107 bytes"));
        assert_eq!(m.naming.as_ref().unwrap().text.len(), 108);
    }
    #[test]
    fn bracketed_paste_cannot_accept_a_name() {
        let mut decoder = Decoder::default();
        let keys = decoder.feed("\x1b[200~hello\nworld jkhlqdcnолрдйвст\x1b[201~".as_bytes());
        assert!(keys.iter().all(|key| matches!(key, Key::Paste(_))));
        let mut m = model();
        m.naming = Some(Naming {
            text: String::new(),
            cursor: 0,
        });
        for key in keys {
            assert!(m.key(key, 75, Layout::new(24, m.focus), None).is_none());
        }
        assert_eq!(
            m.naming.as_ref().unwrap().text,
            "hello world jkhlqdcnолрдйвст"
        );
        assert_eq!(decoder.feed(b"\r"), [Key::Enter]);
    }
    #[test]
    fn pasted_escapes_are_data_and_consecutive_escapes_cancel_then_exit() {
        let input =
            b"\x1b[200~x\x1b[H\x1b!A\n\x1b[2~\x1b[3~\x1bOQ\x1b[12~\x1b[1Q\x1b[1;1Q\x1b\x1b[201~";
        for chunk in 1..=input.len() {
            let mut decoder = Decoder::default();
            let keys = input
                .chunks(chunk)
                .flat_map(|part| decoder.feed(part))
                .collect::<Vec<_>>();
            assert!(keys.iter().all(|key| matches!(key, Key::Paste(_))));
            let mut m = model();
            m.naming = Some(Naming {
                text: String::new(),
                cursor: 0,
            });
            for key in keys {
                assert!(
                    m.key(key, 75, Layout::new(24, m.focus), Some(b'A'))
                        .is_none()
                );
            }
            assert_eq!(
                m.naming.as_ref().unwrap().text,
                "x[H!A [2~[3~OQ[12~[1Q[1;1Q"
            );
            assert!(!decoder.paste);
        }
        let mut decoder = Decoder::default();
        decoder.feed(b"\x1b[200~\x1b");
        decoder.since = Instant::now().checked_sub(Duration::from_secs(1));
        assert_eq!(decoder.expire(), None);
        assert!(decoder.feed(b"[201~").is_empty());
        let mut m = model();
        m.naming = Some(Naming {
            text: "keep".into(),
            cursor: 4,
        });
        assert_eq!(decoder.feed(b"\x1b\x1b"), [Key::Escape]);
        assert!(
            m.key(Key::Escape, 75, Layout::new(24, m.focus), None)
                .is_none()
        );
        assert!(m.naming.is_none());
        decoder.since = Instant::now().checked_sub(Duration::from_secs(1));
        assert_eq!(decoder.expire(), Some(Key::Escape));
        assert!(matches!(
            m.key(Key::Escape, 75, Layout::new(24, m.focus), None),
            Some(Choice::Exit)
        ));
    }
    #[test]
    fn deletion_merges_graphemes_safely_and_hidden_name_controls_are_rejected() {
        let mut m = model();
        for delete in [Key::Delete, Key::Backspace] {
            m.naming = Some(Naming {
                text: "🇺x🇸".into(),
                cursor: 0,
            });
            m.name_key(Key::Home);
            m.name_key(Key::Right);
            if delete == Key::Backspace {
                m.name_key(Key::Right);
            }
            m.name_key(delete);
            assert_eq!(m.naming.as_ref().unwrap().text, "🇺🇸");
            assert_eq!(m.naming.as_ref().unwrap().cursor, "🇺🇸".len());
            m.name_key(Key::Backspace);
            assert!(m.naming.as_ref().unwrap().text.is_empty());
        }
        m.naming = Some(Naming {
            text: "visible".into(),
            cursor: 7,
        });
        for control in ['\u{202e}', '\u{2066}'] {
            m.name_key(Key::Char(control));
            assert_eq!(m.naming.as_ref().unwrap().text, "visible");
            assert!(m.message.contains("directional"));
            assert!(m.available(&format!("hidden{control}")).is_err());
        }
        assert_eq!(inline(&m.naming.as_ref().unwrap().text), "visible");
    }
    #[test]
    fn tiny_geometry_cannot_create_an_unseen_name() {
        let mut m = model();
        for (width, height) in [(75, 1), (75, 2), (5, 24)] {
            for key in [Key::Insert, Key::Char('n'), Key::Char('т')] {
                assert!(
                    m.key(key, width, Layout::new(height, m.focus), None)
                        .is_none()
                );
                assert!(m.naming.is_none());
                assert!(m.message.contains("Resize"));
            }
            m.naming = Some(Naming {
                text: "suggested".into(),
                cursor: 9,
            });
            assert!(
                m.key(Key::Enter, width, Layout::new(height, m.focus), None)
                    .is_none()
            );
            if height < 3 {
                let mut host = vt100::Parser::new(u16::try_from(height).unwrap(), 80, 0);
                host.process(&m.render(80, height));
                assert!(host.screen().contents().contains("Esc cancel"));
            }
            m.name_key(Key::Escape);
        }
    }
    #[test]
    fn ambiguous_grapheme_widths_keep_cells_borders_and_name_cursor_stable() {
        assert_eq!(display_width("❤️"), 2);
        assert_eq!(display_width("👩‍💻"), 4);
        let mut canvas = Canvas::new(79, 24);
        canvas.pane(2, "Unicode", &["A❤️B👩‍💻C".into()], true, None);
        let bytes = canvas.finish();
        let mut host = vt100::Parser::new(24, 80, 0);
        host.process(&bytes);
        assert_eq!(host.screen().cell(2, 5).unwrap().contents(), "B");
        assert_eq!(host.screen().cell(2, 10).unwrap().contents(), "C");
        assert_eq!(host.screen().cell(2, 78).unwrap().contents(), "│");
        assert!(host.screen().cell(2, 79).unwrap().contents().is_empty());
        assert!(std::str::from_utf8(&bytes).unwrap().contains("\x1b[3;6H"));
        let mut m = model();
        let text = "❤️👩‍💻".to_string();
        m.naming = Some(Naming {
            cursor: text.len(),
            text,
        });
        host.process(&m.render(80, 24));
        assert_eq!(host.screen().cursor_position(), (21, 11));
        assert!(host.screen().cell(21, 79).unwrap().contents().is_empty());
    }
    #[test]
    fn zero_width_graphemes_have_linear_sized_results() {
        let text = "\u{200b}".repeat(storage::PREVIEW_LIMIT / 3);
        let start = Instant::now();
        assert_eq!(clip(&text, 10), text);
        assert_eq!(logical_lines(&text), [text]);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn ratatui_virtual_previews_use_invoking_size_and_safe_host_coordinates() {
        let mut m = model();
        let bytes =
            b"old redraw\x1b[2J\x1b[1;1HFIRST\x1b[2;1HSECOND\x1b[3;1H\x1b[31mLAST\x1b[0m".to_vec();
        m.raw = [Some(bytes.clone()), Some(bytes.clone())];
        m.beginning = emulated(&bytes, m.virtual_size, true);
        m.ending = m.beginning.clone();
        for (width, height) in [(80, 24), (196, 95)] {
            for focus in [Focus::List, Focus::Beginning, Focus::Ending] {
                m.focus = focus;
                let frame = m.render(width, height);
                assert_eq!(
                    m.virtual_size,
                    (
                        u16::try_from(height).unwrap(),
                        u16::try_from(width).unwrap()
                    )
                );
                let mut screen = screen::Screen::new(width, height);
                screen.feed(&frame);
                screen.assert_spare_column();
                let rows = screen.rows().join("\n");
                assert!(rows.contains("FIRST") && rows.contains("SECOND") && rows.contains("LAST"));
                assert!(!rows.contains("old redraw"));
                let mut host = vt100::Parser::new(
                    u16::try_from(height).unwrap(),
                    u16::try_from(width).unwrap(),
                    0,
                );
                host.process(&frame);
                for row in 0..u16::try_from(height).unwrap() {
                    assert!(
                        host.screen()
                            .cell(row, u16::try_from(width - 1).unwrap())
                            .unwrap()
                            .contents()
                            .is_empty()
                    );
                }
                assert!(
                    host.screen()
                        .cell(1, 0)
                        .unwrap()
                        .contents()
                        .starts_with('┌')
                );
            }
        }
    }
}
