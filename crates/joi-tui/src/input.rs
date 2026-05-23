//! The prompt's line editor: a UTF-8 string plus a caret (a byte index kept on a char boundary) and
//! the edit operations the keymap drives. Pure and unit-tested — rendering and the block cursor live
//! in `ui.rs`. Newline-aware Home/End keep it correct if multiline input is enabled later.

use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Input {
    value: String,
    /// Byte offset of the caret within `value`, always on a char boundary.
    caret: usize,
}

impl Input {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.caret = 0;
    }

    /// Display columns from the line start to the caret (for positioning the terminal cursor).
    pub fn caret_display_col(&self) -> usize {
        UnicodeWidthStr::width(&self.value[..self.caret])
    }

    pub fn insert(&mut self, c: char) {
        self.value.insert(self.caret, c);
        self.caret += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some((i, _)) = self.value[..self.caret].char_indices().next_back() {
            self.value.remove(i);
            self.caret = i;
        }
    }

    pub fn delete(&mut self) {
        if self.caret < self.value.len() {
            self.value.remove(self.caret);
        }
    }

    pub fn left(&mut self) {
        if let Some((i, _)) = self.value[..self.caret].char_indices().next_back() {
            self.caret = i;
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.value[self.caret..].chars().next() {
            self.caret += c.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.caret = self.value[..self.caret].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.caret = self.value[self.caret..]
            .find('\n')
            .map_or(self.value.len(), |i| self.caret + i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_caret_track_bytes_and_columns() {
        let mut i = Input::default();
        for c in "héy".chars() {
            i.insert(c);
        }
        assert_eq!(i.value(), "héy");
        // 'é' is 2 bytes but 1 column.
        assert_eq!(i.caret, 4);
        assert_eq!(i.caret_display_col(), 3);
    }

    #[test]
    fn backspace_removes_previous_char_including_multibyte() {
        let mut i = Input::default();
        "aé".chars().for_each(|c| i.insert(c));
        i.backspace();
        assert_eq!(i.value(), "a");
        assert_eq!(i.caret, 1);
    }

    #[test]
    fn left_right_move_by_char() {
        let mut i = Input::default();
        "aé".chars().for_each(|c| i.insert(c));
        i.left(); // before 'é'
        assert_eq!(i.caret, 1);
        i.left(); // before 'a'
        assert_eq!(i.caret, 0);
        i.left(); // clamp at start
        assert_eq!(i.caret, 0);
        i.right();
        assert_eq!(i.caret, 1);
    }

    #[test]
    fn home_end_respect_line_boundaries() {
        let mut i = Input::default();
        "ab\ncd".chars().for_each(|c| i.insert(c));
        i.home(); // start of "cd" line
        assert_eq!(i.caret, 3);
        i.end(); // end of "cd" line (= end of value)
        assert_eq!(i.caret, 5);
        // caret in the first line:
        i.caret = 1;
        i.home();
        assert_eq!(i.caret, 0);
        i.end();
        assert_eq!(i.caret, 2); // before '\n'
    }
}
