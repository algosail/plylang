/// Byte range into the source. `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start: start as u32,
            end: end as u32,
        }
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// 1-based line and column. The column counts Unicode scalar values.
pub fn line_col(source: &str, offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let mut line = 1u32;
    let mut col = 1u32;
    for (index, ch) in source.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Byte offset of a 0-based UTF-16 LSP position.
pub fn byte_offset(source: &str, line: u32, character: u32) -> u32 {
    let mut current = 0u32;
    let mut index = 0usize;
    let mut utf16 = 0u32;
    for ch in source.chars() {
        if current == line && utf16 >= character {
            return index as u32;
        }
        if ch == '\n' {
            if current == line {
                return index as u32;
            }
            current += 1;
            utf16 = 0;
            index += ch.len_utf8();
            continue;
        }
        if current == line {
            let width = ch.len_utf16() as u32;
            if utf16 + width > character && utf16 < character {
                return index as u32;
            }
            utf16 += width;
        }
        index += ch.len_utf8();
    }
    index as u32
}

/// 0-based line and UTF-16 column, as LSP positions use them.
pub fn utf16_position(source: &str, offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let mut line = 0u32;
    let mut col = 0u32;
    let mut index = 0usize;
    for ch in source.chars() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
        index += ch.len_utf8();
    }
    (line, col)
}
