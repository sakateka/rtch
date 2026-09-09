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
        for &c in bytes {
            if self.state == 0 {
                if c == ESC {
                    self.seq.clear();
                    self.seq.push(c);
                    self.state = 1;
                    self.overflow = false;
                } else if c != 5 {
                    out.push(c);
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
                    out.extend_from_slice(&self.seq);
                }
                self.state = 0;
                self.seq.clear();
            }
        }
        out
    }
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
    fn text_and_oversized_controls() {
        let mut f = Filter::default();
        assert_eq!(f.feed("Привет\r\n".as_bytes()), "Привет\r\n".as_bytes());
        let mut s = b"\x1b]0;".to_vec();
        s.extend(vec![b'x'; 5000]);
        s.extend(b"\x07OK");
        assert_eq!(f.feed(&s), b"OK");
    }
}
