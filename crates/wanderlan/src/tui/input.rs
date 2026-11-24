//! Single-line text input used by forms, the setup wizard and filters.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a key did to a [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The key is not an editing key.
    Ignored,
    /// Only the cursor moved.
    Moved,
    /// The text changed.
    Edited,
}

/// A single line of editable text with a cursor (counted in characters).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    cursor: usize,
}

impl TextInput {
    /// Creates an input with the cursor at the end of the text.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    /// The current text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replaces the text and moves the cursor to the end.
    pub fn set(&mut self, value: impl Into<String>) {
        *self = Self::new(value);
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.value
            .char_indices()
            .nth(chars)
            .map_or(self.value.len(), |(i, _)| i)
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }

    /// Applies an editing key.
    pub fn handle(&mut self, key: &KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                let end = self.byte_index(self.cursor);
                self.value.replace_range(..end, "");
                self.cursor = 0;
                Outcome::Edited
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                Outcome::Moved
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.len();
                Outcome::Moved
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let at = self.byte_index(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
                Outcome::Edited
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = self.byte_index(self.cursor - 1);
                self.value.remove(at);
                self.cursor -= 1;
                Outcome::Edited
            }
            KeyCode::Delete if self.cursor < self.len() => {
                let at = self.byte_index(self.cursor);
                self.value.remove(at);
                Outcome::Edited
            }
            KeyCode::Backspace | KeyCode::Delete => Outcome::Moved,
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Outcome::Moved
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.len());
                Outcome::Moved
            }
            KeyCode::Home => {
                self.cursor = 0;
                Outcome::Moved
            }
            KeyCode::End => {
                self.cursor = self.len();
                Outcome::Moved
            }
            _ => Outcome::Ignored,
        }
    }

    /// The part of the text to show in a field `width` characters wide, and
    /// the cursor column within it. Long texts scroll so the cursor stays
    /// visible.
    pub fn window(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let start = (self.cursor + 1).saturating_sub(width);
        let text: String = self.value.chars().skip(start).take(width).collect();
        (text, self.cursor - start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_at_the_cursor() {
        let mut input = TextInput::new("hem");
        assert_eq!(input.handle(&key(KeyCode::Left)), Outcome::Moved);
        assert_eq!(input.handle(&key(KeyCode::Char('i'))), Outcome::Edited);
        assert_eq!(input.value(), "heim");
        assert_eq!(input.window(10).1, 3);
        input.handle(&key(KeyCode::Home));
        input.handle(&key(KeyCode::Delete));
        assert_eq!(input.value(), "eim");
        input.handle(&key(KeyCode::End));
        input.handle(&key(KeyCode::Backspace));
        assert_eq!(input.value(), "ei");
        assert_eq!(input.handle(&key(KeyCode::F(1))), Outcome::Ignored);
    }

    #[test]
    fn ctrl_u_clears_before_cursor() {
        let mut input = TextInput::new("10-50");
        input.handle(&key(KeyCode::Left));
        input.handle(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input.value(), "0");
        assert_eq!(input.window(10).1, 0);
    }

    #[test]
    fn window_keeps_cursor_visible() {
        let input = TextInput::new("abcdefghij");
        assert_eq!(input.window(4), ("hij".to_string(), 3));
        let short = TextInput::new("ab");
        assert_eq!(short.window(10), ("ab".to_string(), 2));
    }
}
