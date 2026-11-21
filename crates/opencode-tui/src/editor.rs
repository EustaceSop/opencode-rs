use unicode_width::UnicodeWidthChar;

// Multi-line text buffer behind the prompt and dialog inputs. `cursor` is a byte offset on a char boundary.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    // Other end of the selection, if any.
    pub anchor: Option<usize>,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    pub fn with_text(text: &str) -> Self {
        Self { text: text.to_string(), cursor: text.len(), ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn snapshot(&mut self) {
        if self.undo.last().is_none_or(|(text, _)| *text != self.text) {
            self.undo.push((self.text.clone(), self.cursor));
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    pub fn set(&mut self, text: &str) {
        self.snapshot();
        self.anchor = None;
        self.text = text.to_string();
        self.cursor = self.text.len();
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor.filter(|a| *a != self.cursor)?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(start, end)| &self.text[start..end])
    }

    // Starts or keeps a selection; the next motion extends it.
    pub fn extend(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    fn take_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.anchor = None;
        self.remove(start, end);
        true
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.snapshot();
        if let Some((start, end)) = self.selection() {
            self.text.replace_range(start..end, "");
            self.cursor = start;
        }
        self.anchor = None;
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        self.snapshot();
        self.anchor = None;
        self.text.replace_range(start..end, text);
        self.cursor = start + text.len();
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..].find('\n').map_or(self.text.len(), |i| at + i)
    }

    fn word_end(&self, at: usize) -> usize {
        let mut pos = at;
        let mut chars = self.text[at..].chars().peekable();
        while let Some(c) = chars.next_if(|c| !is_word(*c)) {
            pos += c.len_utf8();
        }
        while let Some(c) = chars.next_if(|c| is_word(*c)) {
            pos += c.len_utf8();
        }
        pos
    }

    fn word_start(&self, at: usize) -> usize {
        let mut pos = at;
        let mut chars = self.text[..at].chars().rev().peekable();
        while let Some(c) = chars.next_if(|c| !is_word(*c)) {
            pos -= c.len_utf8();
        }
        while let Some(c) = chars.next_if(|c| is_word(*c)) {
            pos -= c.len_utf8();
        }
        pos
    }

    fn remove(&mut self, start: usize, end: usize) {
        if start < end {
            self.snapshot();
            self.text.replace_range(start..end, "");
            self.cursor = start;
        }
    }

    pub fn backspace(&mut self) {
        if !self.take_selection() {
            self.remove(self.prev(self.cursor), self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if !self.take_selection() {
            self.remove(self.cursor, self.next(self.cursor));
        }
    }

    pub fn visual_line_home(&mut self, width: usize) {
        let (index, _) = self.cursor_position(width);
        self.cursor = self.rows(width)[index].start;
    }

    pub fn visual_line_end(&mut self, width: usize) {
        let (index, _) = self.cursor_position(width);
        self.cursor = self.rows(width)[index].end;
    }

    pub fn delete_word_backward(&mut self) {
        self.remove(self.word_start(self.cursor), self.cursor);
    }

    pub fn delete_word_forward(&mut self) {
        self.remove(self.cursor, self.word_end(self.cursor));
    }

    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        // At the end of a line the newline itself goes, joining the next line.
        self.remove(self.cursor, if end == self.cursor { self.next(end) } else { end });
    }

    pub fn delete_to_line_start(&mut self) {
        self.remove(self.line_start(self.cursor), self.cursor);
    }

    pub fn delete_line(&mut self) {
        let start = self.line_start(self.cursor);
        let end = self.line_end(self.cursor);
        if end < self.text.len() {
            self.remove(start, end + 1);
        } else {
            self.remove(start.saturating_sub(usize::from(start > 0)), end);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.prev(self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = self.next(self.cursor);
    }

    pub fn word_forward(&mut self) {
        self.cursor = self.word_end(self.cursor);
    }

    pub fn word_backward(&mut self) {
        self.cursor = self.word_start(self.cursor);
    }

    pub fn line_home(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn line_end_move(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    pub fn buffer_home(&mut self) {
        self.cursor = 0;
    }

    pub fn buffer_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn undo(&mut self) {
        if let Some((text, cursor)) = self.undo.pop() {
            self.redo.push((std::mem::replace(&mut self.text, text), self.cursor));
            self.cursor = cursor.min(self.text.len());
        }
    }

    pub fn redo(&mut self) {
        if let Some((text, cursor)) = self.redo.pop() {
            self.undo.push((std::mem::replace(&mut self.text, text), self.cursor));
            self.cursor = cursor.min(self.text.len());
        }
    }

    // Visual rows after hard-wrapping every logical line at `width` columns.
    pub fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut offset = 0;
        for line in self.text.split('\n') {
            let mut start = offset;
            let mut used = 0;
            for (i, c) in line.char_indices() {
                let w = c.width().unwrap_or(0);
                if used + w > width && offset + i > start {
                    rows.push(Row { start, end: offset + i });
                    start = offset + i;
                    used = 0;
                }
                used += w;
            }
            rows.push(Row { start, end: offset + line.len() });
            offset += line.len() + 1;
        }
        rows
    }

    fn column(&self, row: Row, at: usize) -> usize {
        self.text[row.start..at.min(row.end)].chars().map(|c| c.width().unwrap_or(0)).sum()
    }

    // (row, column) of the cursor among `rows(width)`.
    pub fn cursor_position(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width);
        let index = rows.iter().rposition(|row| row.start <= self.cursor).unwrap_or(0);
        (index, self.column(rows[index], self.cursor))
    }

    // Moves one visual row; false when already on the first/last row.
    pub fn move_vertical(&mut self, direction: i32, width: usize) -> bool {
        let rows = self.rows(width);
        let (index, column) = self.cursor_position(width);
        let target = index as i64 + i64::from(direction);
        if target < 0 || target >= rows.len() as i64 {
            return false;
        }
        let row = rows[target as usize];
        let mut used = 0;
        let mut at = row.start;
        for c in self.text[row.start..row.end].chars() {
            let w = c.width().unwrap_or(0);
            if used + w > column {
                break;
            }
            used += w;
            at += c.len_utf8();
        }
        self.cursor = at;
        true
    }

    // The whitespace-delimited token that ends at the cursor, with its start offset.
    pub fn token_before_cursor(&self) -> (usize, &str) {
        let before = &self.text[..self.cursor];
        let start = before.rfind(char::is_whitespace).map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
        (start, &before[start..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_undo() {
        let mut e = Editor::default();
        e.insert("hello world");
        e.delete_word_backward();
        assert_eq!(e.text, "hello ");
        e.insert("there\nsecond line");
        e.line_home();
        e.delete_to_line_end();
        assert_eq!(e.text, "hello there\n");
        e.undo();
        assert_eq!(e.text, "hello there\nsecond line");
        e.redo();
        assert_eq!(e.text, "hello there\n");
        e.buffer_home();
        e.word_forward();
        assert_eq!(e.cursor, 5);
        e.delete_to_line_start();
        assert_eq!(e.text, " there\n");
        e.delete_line();
        assert_eq!(e.text, "");
    }

    #[test]
    fn multibyte_cursor_stays_on_boundaries() {
        let mut e = Editor::with_text("añ漢");
        e.move_left();
        e.backspace();
        assert_eq!(e.text, "a漢");
        e.buffer_home();
        e.move_right();
        e.delete();
        assert_eq!(e.text, "a");
    }

    #[test]
    fn visual_rows_and_vertical_movement() {
        let mut e = Editor::with_text("abcdefgh\nxy");
        assert_eq!(e.rows(4), vec![Row { start: 0, end: 4 }, Row { start: 4, end: 8 }, Row { start: 9, end: 11 }]);
        assert_eq!(e.cursor_position(4), (2, 2));
        assert!(e.move_vertical(-1, 4));
        assert_eq!(e.cursor, 6);
        assert!(e.move_vertical(-1, 4));
        assert!(!e.move_vertical(-1, 4));
        assert_eq!(e.cursor, 2);
        e.buffer_end();
        assert!(!e.move_vertical(1, 4));
        e.cursor = 6;
        e.visual_line_home(4);
        assert_eq!(e.cursor, 4);
        e.visual_line_end(4);
        assert_eq!(e.cursor, 8);
    }

    #[test]
    fn selection_extends_replaces_and_deletes() {
        let mut e = Editor::with_text("hello world");
        e.extend();
        e.word_backward();
        assert_eq!(e.selected_text(), Some("world"));
        e.extend();
        e.word_backward();
        assert_eq!(e.selection(), Some((0, 11)));
        e.insert("x");
        assert_eq!((e.text.as_str(), e.selection()), ("x", None));
        e.undo();
        assert_eq!(e.text, "hello world");
        e.select_all();
        e.backspace();
        assert_eq!(e.text, "");
        e.insert("ab");
        e.extend();
        e.move_left();
        e.delete();
        assert_eq!(e.text, "a");
    }

    #[test]
    fn finds_token_before_cursor() {
        let e = Editor::with_text("look at @src/ma");
        assert_eq!(e.token_before_cursor(), (8, "@src/ma"));
        assert_eq!(Editor::with_text("/mod").token_before_cursor(), (0, "/mod"));
    }
}
