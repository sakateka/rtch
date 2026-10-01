use unicode_width::UnicodeWidthChar;

/// Test decoder for cursor addressing and cell writes emitted by the picker.
pub struct Screen {
    width: usize,
    height: usize,
    cells: Vec<Vec<String>>,
    row: usize,
    column: usize,
    pub writes: Vec<(usize, usize, usize)>,
}
impl Screen {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![vec![String::from(" "); width]; height],
            row: 0,
            column: 0,
            writes: Vec::new(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes).expect("frame is valid UTF-8");
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.next() == Some('[') {
                let mut parameters = String::new();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        if c == 'H' {
                            let mut numbers = parameters
                                .split(';')
                                .map(|s| s.parse::<usize>().unwrap_or(1));
                            self.row = numbers.next().unwrap_or(1).saturating_sub(1);
                            self.column = numbers.next().unwrap_or(1).saturating_sub(1);
                        } else if c == 'K' && parameters == "2" && self.row < self.height {
                            self.cells[self.row].fill(String::from(" "));
                        }
                        break;
                    }
                    parameters.push(c);
                }
                continue;
            }
            if c.is_control() {
                continue;
            }
            let size = c.width().unwrap_or(0);
            if size == 0 {
                if self.row < self.height && self.column > 0 && self.column <= self.width {
                    self.cells[self.row][self.column - 1].push(c);
                }
                continue;
            }
            self.writes.push((self.row + 1, self.column + 1, size));
            assert!(
                self.row < self.height && self.column + size <= self.width,
                "out-of-bounds terminal write"
            );
            self.cells[self.row][self.column] = c.to_string();
            for column in self.column + 1..self.column + size {
                self.cells[self.row][column].clear();
            }
            self.column += size;
        }
    }
    pub fn row(&self, row: usize) -> String {
        self.cells[row - 1].join("")
    }
    pub fn rows(&self) -> Vec<String> {
        (1..=self.height).map(|row| self.row(row)).collect()
    }
    pub fn assert_spare_column(&self) {
        assert!(
            self.writes
                .iter()
                .all(|&(row, column, size)| row <= self.height && column + size <= self.width)
        );
        assert!(
            self.cells
                .iter()
                .all(|row| row.last().is_some_and(|cell| cell == " "))
        );
    }
}
