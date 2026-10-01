//! Filter replay, never the live terminal stream. No terminal emulation.
#[derive(Default)]
pub struct Filter {
    seq: Vec<u8>,
    state: u8,
    overflow: bool,
}
const ESC: u8 = 27;
impl Filter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + self.seq.len());
        self.feed_into(bytes, &mut out);
        out
    }

    pub fn feed_into(&mut self, bytes: &[u8], out: &mut impl Extend<u8>) {
        for &c in bytes {
            if self.state == 0 {
                if c == ESC {
                    self.seq.clear();
                    self.seq.push(c);
                    self.state = 1;
                    self.overflow = false;
                } else if c != 5 {
                    out.extend([c]);
                }
                continue;
            }
            if c == ESC && matches!(self.state, 1 | 2 | 6) {
                self.seq.clear();
                self.seq.push(c);
                self.state = 1;
                self.overflow = false;
                continue;
            }
            if c == 24 || c == 26 {
                self.state = 0;
                self.seq.clear();
                continue;
            }
            if self.seq.len() < 4096 {
                self.seq.push(c);
            } else {
                self.overflow = true;
            }
            let mut done = false;
            let mut keep = false;
            match self.state {
                1 => match c {
                    b'[' => self.state = 2,
                    b']' => self.state = 3,
                    b'P' | b'_' | b'^' | b'X' => self.state = 4,
                    0x20..=0x2f => self.state = 6,
                    _ => {
                        done = true;
                        keep = c != b'Z';
                    }
                },
                2 if (0x40..=0x7e).contains(&c) => {
                    done = true;
                    keep = !(b"cntxp".contains(&c) || c == b'u' && self.seq.get(2) == Some(&b'?'));
                }
                3 | 4 => {
                    if c == ESC {
                        self.state = 5;
                    } else if c == 7 && self.seq.get(1) == Some(&b']') {
                        done = true;
                    }
                }
                5 => {
                    if c == b'\\' {
                        done = true;
                    } else if c != ESC {
                        self.state = if self.seq.get(1) == Some(&b']') { 3 } else { 4 };
                    }
                }
                6 if (0x30..=0x7e).contains(&c) => {
                    done = true;
                    keep = true;
                }
                _ => {}
            }
            if done {
                if self.seq.get(1) == Some(&b']') {
                    keep = !self
                        .seq
                        .windows(3)
                        .any(|w| w[0] == b';' && w[1] == b'?' && [b';', 7, ESC].contains(&w[2]));
                }
                if keep && !self.overflow {
                    out.extend(self.seq.iter().copied());
                }
                self.state = 0;
                self.seq.clear();
            }
        }
    }
}

/// Iterate valid UTF-8 atomically; standalone 8-bit C1 bytes remain controls.
struct PreviewChars<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl Iterator for PreviewChars<'_> {
    type Item = (usize, char);
    fn next(&mut self) -> Option<Self::Item> {
        let start = self.position;
        let &byte = self.bytes.get(start)?;
        let size = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        if let Some(part) = self.bytes.get(start..start + size)
            && let Ok(text) = std::str::from_utf8(part)
        {
            self.position += size;
            return Some((start, text.chars().next().expect("one character")));
        }
        self.position += 1;
        Some((start, if byte < 0xa0 { char::from(byte) } else { '�' }))
    }
}
#[derive(Default)]
struct PreviewFilter {
    state: u8,
    cr: bool,
}
impl PreviewFilter {
    /// Return printable text and whether an unmatched string terminator occurred.
    fn character(&mut self, c: char) -> (Option<char>, bool) {
        if self.state != 0 && matches!(c, '\u{18}' | '\u{1a}') {
            self.state = 0;
            return (None, false);
        }
        let mut unmatched = false;
        let mut output = None;
        match self.state {
            0 => match c {
                '\u{1b}' => self.state = 1,
                '\u{9b}' => self.state = 2,
                '\u{9d}' => self.state = 3,
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => self.state = 4,
                '\u{9c}' => unmatched = true,
                '\r' => {
                    output = Some('\n');
                    self.cr = true;
                }
                '\n' => {
                    if !self.cr {
                        output = Some('\n');
                    }
                    self.cr = false;
                }
                '\t' => {
                    output = Some(c);
                    self.cr = false;
                }
                _ if c.is_control()
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => {}
                _ => {
                    output = Some(c);
                    self.cr = false;
                }
            },
            1 => {
                unmatched = c == '\\';
                self.state = match c {
                    '[' => 2,
                    ']' => 3,
                    'P' | 'X' | '^' | '_' => 4,
                    '\u{1b}' => 1,
                    '\u{20}'..='\u{2f}' => 6,
                    _ => 0,
                };
            }
            2 => {
                if c == '\u{1b}' {
                    self.state = 1;
                } else if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.state = 0;
                }
            }
            3 | 4 => {
                if c == '\u{1b}' {
                    self.state += 4;
                } else if c == '\u{9c}' || c == '\u{7}' && self.state == 3 {
                    self.state = 0;
                }
            }
            7 | 8 => {
                if c == '\\' || c == '\u{9c}' {
                    self.state = 0;
                } else if c != '\u{1b}' {
                    self.state -= 4;
                }
            }
            _ => {
                if ('\u{30}'..='\u{7e}').contains(&c) {
                    self.state = 0;
                }
            }
        }
        (output, unmatched)
    }
}
/// Plain, safe picker text. Unlike replay filtering, no terminal controls survive.
pub fn preview(bytes: &[u8]) -> String {
    let mut filter = PreviewFilter::default();
    PreviewChars { bytes, position: 0 }
        .filter_map(|(_, c)| filter.character(c).0)
        .collect()
}
/// Use bounded earlier context to avoid starting a suffix within a known control.
/// Unmatched string terminators discard a possible orphaned initial payload.
pub fn suffix_boundary(bytes: &[u8], requested: usize, truncated: bool) -> usize {
    let requested = if truncated {
        bytes[requested..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| requested + i + 1)
    } else {
        requested
    };
    let mut filter = PreviewFilter::default();
    let mut boundary = None;
    for (offset, c) in (PreviewChars { bytes, position: 0 }) {
        if offset >= requested && filter.state == 0 && boundary.is_none() {
            boundary = Some(offset);
        }
        if filter.character(c).1 {
            boundary = None;
        }
    }
    boundary.unwrap_or(bytes.len())
}
/// Keep the earliest complete line or escape, including newline-free redraws.
pub fn emulator_boundary(bytes: &[u8], truncated: bool) -> usize {
    if !truncated {
        return 0;
    }
    let mut filter = PreviewFilter::default();
    let mut boundary = None;
    let mut chars = PreviewChars { bytes, position: 0 };
    while let Some((offset, c)) = chars.next() {
        if filter.state == 0 && boundary.is_none() {
            if c == '\u{1b}'
                && bytes
                    .get(offset + 1)
                    .is_some_and(|b| (0x20..=0x7e).contains(b))
                || matches!(c, '\u{90}' | '\u{98}' | '\u{9b}'..='\u{9f}')
            {
                boundary = Some(offset);
            } else if c == '\n' {
                boundary = Some(chars.position);
            }
        }
        if filter.character(c).1 {
            // The opener was lost: drop possible payload before its terminator.
            boundary = Some(chars.position);
        }
    }
    boundary.unwrap_or(0)
}
/// Replay only into a bounded virtual screen; no terminal sequences reach the host.
pub fn terminal_preview(bytes: &[u8], rows: u16, columns: u16, beginning: bool) -> String {
    let rows = rows.clamp(1, 200);
    let columns = columns.clamp(1, 512);
    let mut normalized = Vec::with_capacity(bytes.len());
    for (_, c) in (PreviewChars { bytes, position: 0 }) {
        if ('\u{80}'..='\u{9f}').contains(&c) {
            normalized.extend([27, u8::try_from(u32::from(c) - 0x40).expect("C1 control")]);
        } else {
            let mut encoded = [0; 4];
            normalized.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
        }
    }
    let mut parser = vt100::Parser::new(rows, columns, 1024);
    if beginning {
        for byte in normalized {
            parser.process(&[byte]);
            parser.screen_mut().set_scrollback(usize::MAX);
            let count = parser.screen().scrollback();
            parser.screen_mut().set_scrollback(0);
            // A single terminal action can scroll at most one screen. Leave
            // that headroom and stop before the first retained row is evicted.
            if count >= 1024 - usize::from(rows) {
                break;
            }
        }
    } else {
        parser.process(&normalized);
    }
    parser.screen_mut().set_scrollback(usize::MAX);
    let mut offset = parser.screen().scrollback();
    let mut lines: Vec<String> = Vec::new();
    let mut continued = false;
    loop {
        parser.screen_mut().set_scrollback(offset);
        let take = if offset == 0 {
            usize::from(rows)
        } else {
            offset.min(usize::from(rows))
        };
        for (row, text) in parser.screen().rows(0, columns).take(take).enumerate() {
            let text = preview(text.as_bytes());
            if continued {
                lines.last_mut().expect("continued row").push_str(&text);
            } else {
                lines.push(text);
            }
            // The marker belongs to this row and continues into the next,
            // including across scrollback chunks and the live-screen seam.
            continued = parser
                .screen()
                .row_wrapped(u16::try_from(row).expect("bounded row"));
        }
        if offset == 0 {
            break;
        }
        offset -= take;
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_queries_split_at_every_boundary() {
        let input=b"A\x1b[6n\x1b]10;?\x1b\\\x1b]11;?\x07\x1b[?u\x1b[c\x1bP$qm\x1b\\\x1b_Ga=q\x1b\\\x1b[35mOK\x1b[0m";
        for n in 1..=input.len() {
            let mut f = Filter::default();
            let out: Vec<_> = input.chunks(n).flat_map(|p| f.feed(p)).collect();
            assert_eq!(out, b"A\x1b[35mOK\x1b[0m");
        }
    }
    #[test]
    fn preview_strips_controls_without_changing_replay() {
        let input = b"A\x1b[31mB\x1b[0m\x1b]0;TITLE\x07\x1bPSECRET\x1b\\\x1b_GPAYLOAD\x1b\\\x1b(B\x00\x07\x08\x7f\r\nC\tD";
        assert_eq!(preview(input), "AB\nC\tD");
        assert_eq!(
            preview("Привет 界 👩‍💻 \u{202e}safe".as_bytes()),
            "Привет 界 👩‍💻 safe"
        );
        assert_eq!(preview(b"a\xffb"), "a�b");
        assert_eq!(preview(b"a\x1b]incomplete"), "a");
        assert_eq!(preview("a\u{9b}31mb\u{9d}secret\u{9c}c".as_bytes()), "abc");
    }
    #[test]
    fn text_and_oversized_controls() {
        let mut f = Filter::default();
        assert_eq!(f.feed("Привет\r\n".as_bytes()), "Привет\r\n".as_bytes());
        let mut s = b"\x1b]0;".to_vec();
        s.extend(vec![b'x'; 5000]);
        s.extend(b"\x07OK");
        assert_eq!(f.feed(&s), b"OK");
    }
    #[test]
    fn raw_c1_and_multiline_strings_preserve_utf8() {
        let raw = b"A\x9b31mB\x9dtitle\nSECRET\x9cC\x90payload\nSECRET\x9cD";
        assert_eq!(preview(raw), "ABCD");
        assert_eq!(
            preview("Ж界👩‍💻\u{9d}secret\nsecret\u{9c}OK".as_bytes()),
            "Ж界👩‍💻OK"
        );
        assert_eq!(
            preview(b"A\x1b]0;title\nsecret\x07B\x1bPpayload\nsecret\x1b\\C"),
            "ABC"
        );
    }
    #[test]
    fn virtual_screen_applies_cursor_erase_backspace_and_sgr() {
        let redraw = b"obsolete garbage\x1b[2J\x1b[1;1HHELLO\x1b[1;3HX\x1b[2;1Hsecond\x08!\x1b[K";
        assert_eq!(terminal_preview(redraw, 4, 20, false), "HEXLO\nsecon!");
        assert_eq!(
            terminal_preview(
                b"\x1b[31mRED\x1b[0m\x1b]title\nSECRET\x07\x90DCS\nSECRET\x9c",
                4,
                20,
                false
            ),
            "RED"
        );
        assert_eq!(terminal_preview("Ж界👩‍💻".as_bytes(), 4, 20, false), "Ж界👩‍💻");
        assert_eq!(terminal_preview(b"VISIBLE\x1b[2K", 4, 20, false), "");
    }
    #[test]
    fn virtual_screen_alternate_and_scrollback_are_bounded() {
        let alternate = b"MAIN\x1b[?1049h\x1b[1;1HALT\x1b[2;1HSCREEN";
        let text = terminal_preview(alternate, 4, 20, false);
        assert!(text.contains("ALT") && text.contains("SCREEN") && !text.contains("MAIN"));
        let mut closed = alternate.to_vec();
        closed.extend(b"\x1b[?1049l");
        assert_eq!(terminal_preview(&closed, 4, 20, false), "MAIN");
        let lines = (0..8)
            .map(|i| format!("line-{i}\r\n"))
            .collect::<Vec<_>>()
            .concat();
        let text = terminal_preview(lines.as_bytes(), 3, 20, false);
        assert_eq!(text.lines().count(), 8);
        assert!(text.starts_with("line-0") && text.ends_with("line-7"));
    }
    #[test]
    fn soft_wrap_chains_cross_scrollback_chunks_and_live_screen() {
        for rows in [2, 3, 24, 95] {
            for columns in [8, 20, 80] {
                let long = format!("START{} SUFFIX", "x".repeat(600));
                let input = format!("hard-first\r\n{long}\r\nhard-last");
                for beginning in [true, false] {
                    assert_eq!(
                        terminal_preview(input.as_bytes(), rows, columns, beginning),
                        format!("hard-first\n{long}\nhard-last"),
                        "{rows}x{columns}, beginning={beginning}"
                    );
                }
            }
        }
    }
    #[test]
    fn soft_wrap_preserves_spaces_wide_characters_and_hard_blank_rows() {
        for columns in [4, 8] {
            let input = "界界👩‍💻 Z   suffix\r\n\r\nlast";
            assert_eq!(
                terminal_preview(input.as_bytes(), 3, columns, false),
                "界界👩‍💻 Z   suffix\n\nlast"
            );
        }
        assert_eq!(
            terminal_preview("a界e\u{301}👩‍💻 suffix".as_bytes(), 3, 20, false),
            "a界e\u{301}👩‍💻 suffix"
        );
        assert_eq!(terminal_preview(b"1234\r\n5678", 2, 4, false), "1234\n5678");
        assert_eq!(
            terminal_preview("abc界tail".as_bytes(), 3, 4, false),
            "abc\n界tail"
        );
        assert_eq!(
            terminal_preview(b"12345678\x1b[2;2HX", 2, 4, false),
            "12345X78"
        );
    }
    #[test]
    fn beginning_preserves_first_rows_beyond_scrollback_capacity() {
        use std::fmt::Write as _;
        let mut bytes = String::new();
        for i in 0..1250 {
            write!(bytes, "ROW-{i:04}\r\n").unwrap();
        }
        for (rows, columns) in [(24, 80), (95, 196)] {
            let beginning = terminal_preview(bytes.as_bytes(), rows, columns, true);
            assert_eq!(beginning.lines().next(), Some("ROW-0000"));
            assert!(beginning.lines().count() <= 1024);
            assert!(!beginning.contains("ROW-1249"));
            let ending = terminal_preview(bytes.as_bytes(), rows, columns, false);
            assert!(ending.ends_with("ROW-1249"));
            assert!(!ending.contains("ROW-0000"));
        }
    }
    #[test]
    fn emulator_boundary_keeps_lines_before_sgr_and_drops_orphan_payloads() {
        let bytes = b"partial\r\nFIRST\r\nSECOND\r\n\x1b[32mTHIRD";
        assert_eq!(emulator_boundary(bytes, true), 9);
        assert_eq!(
            terminal_preview(&bytes[9..], 24, 80, false),
            "FIRST\nSECOND\nTHIRD"
        );
        for bytes in [
            b"lost payload\nmore\x1b\\SAFE".as_slice(),
            b"payload\nmore\x9cSAFE",
        ] {
            let start = emulator_boundary(bytes, true);
            assert_eq!(terminal_preview(&bytes[start..], 24, 80, false), "SAFE");
        }
        assert_eq!(emulator_boundary(b"partial\x1b[2J\x1b[HFINAL", true), 7);
    }
}
