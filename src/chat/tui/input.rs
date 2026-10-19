//! The message box: a small multi-line editor with history.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    chars: Vec<char>,
    /// Cursor position, in chars.
    cursor: usize,
    history: Vec<String>,
    /// Position while browsing history; `None` when editing a fresh line.
    browsing: Option<usize>,
    /// What was typed before browsing started.
    draft: String,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn is_multiline(&self) -> bool {
        self.chars.contains(&'\n')
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
        self.browsing = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        // Pasted text: normalize line endings, keep tabs readable.
        for c in s.replace("\r\n", "\n").replace('\r', "\n").chars() {
            self.insert(if c == '\t' { ' ' } else { c });
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Start of the current line.
    pub fn home(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    /// End of the current line.
    pub fn end(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    /// Delete the word before the cursor (Ctrl-W).
    pub fn delete_word(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
    }

    /// Delete from the cursor to the end of the line (Ctrl-K).
    pub fn kill_to_end(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor] != '\n' {
            self.chars.remove(self.cursor);
        }
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Take the text for sending and remember it in the history.
    pub fn submit(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.clear();
        text
    }

    fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    /// Up: an earlier message.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = match self.browsing {
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.browsing = Some(i);
        let text = self.history[i].clone();
        self.set(&text);
    }

    /// Down: a later message, then back to what was being typed.
    pub fn history_next(&mut self) {
        let Some(i) = self.browsing else { return };
        if i + 1 < self.history.len() {
            self.browsing = Some(i + 1);
            let text = self.history[i + 1].clone();
            self.set(&text);
        } else {
            self.browsing = None;
            let draft = std::mem::take(&mut self.draft);
            self.set(&draft);
        }
    }

    /// The text wrapped to `width` columns, and the cursor's (row, column).
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let mut cursor = (0, 0);
        for (i, &c) in self.chars.iter().enumerate() {
            if i == self.cursor {
                cursor = (rows.len() - 1, col);
            }
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor == self.chars.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            cursor = (rows.len() - 1, col);
        }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Input {
        let mut i = Input::default();
        i.insert_str(s);
        i
    }

    #[test]
    fn edits() {
        let mut i = typed("hello world");
        i.delete_word();
        assert_eq!(i.text(), "hello ");
        i.home();
        i.insert('>');
        assert_eq!(i.text(), ">hello ");
        i.kill_to_end();
        assert_eq!(i.text(), ">");
        let mut j = typed("ab\r\ncd");
        assert_eq!(j.text(), "ab\ncd");
        j.home();
        j.backspace();
        assert_eq!(j.text(), "abcd");
    }

    #[test]
    fn history_keeps_the_draft() {
        let mut i = Input::default();
        i.insert_str("first");
        i.submit();
        i.insert_str("second");
        i.submit();
        i.insert_str("draft");
        i.history_prev();
        assert_eq!(i.text(), "second");
        i.history_prev();
        i.history_prev();
        assert_eq!(i.text(), "first");
        i.history_next();
        i.history_next();
        assert_eq!(i.text(), "draft");
    }

    #[test]
    fn wraps_and_places_the_cursor() {
        let i = typed("abcdef\nxy");
        let (rows, cursor) = i.layout(4);
        assert_eq!(rows, ["abcd", "ef", "xy"]);
        assert_eq!(cursor, (2, 2));
        let (rows, cursor) = typed("abcd").layout(4);
        assert_eq!(rows, ["abcd", ""]);
        assert_eq!(cursor, (1, 0));
        let (_, cursor) = typed("日本").layout(3);
        assert_eq!(cursor, (1, 2), "wide characters count double");
    }
}
