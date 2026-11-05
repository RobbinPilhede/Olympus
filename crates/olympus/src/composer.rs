/// The message input: a text buffer with a byte-indexed cursor and history.
#[derive(Debug, Default)]
pub struct Composer {
    pub text: String,
    pub cursor: usize,
    /// The fixed end of a selection; `cursor` is the moving end.
    pub anchor: Option<usize>,
    /// Text an input method is still composing.
    pub marked: Option<std::ops::Range<usize>>,
    history: Vec<String>,
    browsing: Option<usize>,
}

impl Composer {
    /// Inserts text at the cursor; line endings are normalized for pastes.
    pub fn insert(&mut self, text: &str) {
        self.delete_selection();
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.browsing = None;
    }

    /// Replaces all text and moves the cursor to the end.
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.anchor = None;
        self.marked = None;
    }

    /// The selected byte range, if anything is selected.
    pub fn selection(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.anchor.filter(|a| *a != self.cursor)?;
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    /// The selected text.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|range| &self.text[range])
    }

    /// Removes the selection; returns whether anything was removed.
    pub fn delete_selection(&mut self) -> bool {
        let selection = self.selection();
        self.anchor = None;
        let Some(range) = selection else { return false };
        self.cursor = range.start;
        self.text.replace_range(range, "");
        true
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Moves the cursor, starting or extending a selection when `extend` is set.
    pub fn move_to(&mut self, offset: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = offset.min(self.text.len());
    }

    /// The start of the word before the cursor.
    pub fn word_left(&self) -> usize {
        let trimmed = self.text[..self.cursor].trim_end();
        trimmed.rfind(char::is_whitespace).map_or(0, |i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
    }

    /// The end of the word after the cursor.
    pub fn word_right(&self) -> usize {
        let rest = &self.text[self.cursor..];
        let skipped = rest.len() - rest.trim_start().len();
        self.cursor + skipped + rest[skipped..].find(char::is_whitespace).unwrap_or(rest.len() - skipped)
    }

    /// Takes the text for sending and records it in history.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.anchor = None;
        self.marked = None;
        self.browsing = None;
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    /// Deletes the character before the cursor.
    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.remove(index);
            self.cursor = index;
        }
    }

    /// Deletes the character at the cursor.
    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    /// Moves left one character.
    pub fn left(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.selection() {
                self.move_to(range.start, false);
                return;
            }
        }
        let index = self.text[..self.cursor].char_indices().next_back().map_or(0, |(index, _)| index);
        self.move_to(index, extend);
    }

    /// Moves right one character.
    pub fn right(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.selection() {
                self.move_to(range.end, false);
                return;
            }
        }
        let width = self.text[self.cursor..].chars().next().map_or(0, char::len_utf8);
        self.move_to(self.cursor + width, extend);
    }

    /// Moves to the start of the current line.
    pub fn home(&mut self, extend: bool) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        self.move_to(start, extend);
    }

    /// Moves to the end of the current line.
    pub fn end(&mut self, extend: bool) {
        let end = self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i);
        self.move_to(end, extend);
    }

    /// Deletes the word before the cursor.
    pub fn delete_word(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.word_left();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Moves back through history when the input is one line.
    pub fn history_up(&mut self) -> bool {
        if self.text.contains('\n') || self.history.is_empty() {
            return false;
        }
        let index = match self.browsing {
            Some(0) => 0,
            Some(i) => i - 1,
            None if self.text.is_empty() => self.history.len() - 1,
            None => return false,
        };
        self.browsing = Some(index);
        let entry = self.history[index].clone();
        self.set(&entry);
        true
    }

    /// Moves forward through history.
    pub fn history_down(&mut self) -> bool {
        let Some(index) = self.browsing else { return false };
        if index + 1 < self.history.len() {
            self.browsing = Some(index + 1);
            let entry = self.history[index + 1].clone();
            self.set(&entry);
        } else {
            self.browsing = None;
            self.set("");
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_respect_multibyte_characters() {
        let mut c = Composer::default();
        c.insert("héllo");
        c.left(false);
        c.left(false);
        c.left(false);
        c.backspace();
        assert_eq!(c.text, "hllo");
        c.delete();
        assert_eq!(c.text, "hlo");
    }

    #[test]
    fn pasted_crlf_becomes_newlines() {
        let mut c = Composer::default();
        c.insert("a\r\nb\rc");
        assert_eq!(c.text, "a\nb\nc");
    }

    #[test]
    fn home_and_end_stay_on_the_current_line() {
        let mut c = Composer::default();
        c.insert("one\ntwo");
        c.home(false);
        assert_eq!(c.cursor, 4);
        c.end(false);
        assert_eq!(c.cursor, 7);
    }

    #[test]
    fn delete_word_removes_the_previous_word() {
        let mut c = Composer::default();
        c.insert("hello brave  ");
        c.delete_word();
        assert_eq!(c.text, "hello ");
    }

    #[test]
    fn history_recalls_and_returns_to_empty() {
        let mut c = Composer::default();
        c.insert("first");
        c.take();
        c.insert("second");
        c.take();
        assert!(c.history_up());
        assert_eq!(c.text, "second");
        assert!(c.history_up());
        assert_eq!(c.text, "first");
        assert!(c.history_down());
        assert_eq!(c.text, "second");
        assert!(c.history_down());
        assert_eq!(c.text, "");
    }

    #[test]
    fn history_never_overwrites_a_draft() {
        let mut c = Composer::default();
        c.insert("sent");
        c.take();
        c.insert("draft");
        assert!(!c.history_up());
        assert_eq!(c.text, "draft");
    }

    #[test]
    fn typing_replaces_a_selection_and_arrows_collapse_it() {
        let mut c = Composer::default();
        c.insert("hello world");
        c.home(true);
        assert_eq!(c.selected_text(), Some("hello world"));
        c.left(false);
        assert_eq!((c.cursor, c.selection()), (0, None));
        c.right(true);
        c.right(true);
        assert_eq!(c.selected_text(), Some("he"));
        c.insert("HE");
        assert_eq!(c.text, "HEllo world");
    }

    #[test]
    fn backspace_and_delete_remove_only_the_selection() {
        let mut c = Composer::default();
        c.insert("abc");
        c.select_all();
        c.backspace();
        assert_eq!((c.text.as_str(), c.cursor), ("", 0));
    }

    #[test]
    fn word_motion_skips_spaces_and_respects_multibyte() {
        let mut c = Composer::default();
        c.insert("åäö  öl då");
        assert_eq!(c.word_left(), "åäö  öl ".len());
        c.move_to(0, false);
        assert_eq!(c.word_right(), "åäö".len());
    }
}
